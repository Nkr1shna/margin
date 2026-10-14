//! LLM-as-judge orchestration (opt-in runner only, §3.2). The prompts, rubrics and
//! canary sets are R1-ai's (`margin_ai::evals`: serious-question generation, the 0–3
//! seriousness grader, the faithfulness/relevance/coverage/terminology judges, absolute
//! and pairwise, and the canaries); this module is what the runner does with them:
//!
//! - every lesson or answer is judged twice, the second time with its passages in the
//!   opposite order (markers renumbered to match), so position bias cancels and the two
//!   passes give an agreement κ;
//! - with two judges (one per provider when both keys are configured) a second κ;
//! - the canaries are run through every judge and grader on every run, and a judge
//!   whose canaries fail invalidates the run (the report says so).
//!
//! Because golden questions and judged outputs get no human review (owner Q8), these
//! numbers are what shows the judging can be trusted.
//!
//! A [`Judge`] is the seam: `ai_judge::AiJudge` builds R1-ai's prompts and sends them
//! through an [`crate::model::EvalModel`] (either provider); tests use fakes.

use std::collections::BTreeMap;

use margin_service::engines::BoxFuture;
use margin_shared::Level;
use serde::{Deserialize, Serialize};

use crate::citations::SentPassage;
use crate::metrics::quadratic_kappa;

/// What a judge grades (R1-ai's `evals::Rubric`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Criterion {
    /// Every source-derived claim against the passages sent; the grade is the lowest
    /// claim's, `item_mean` the mean over claims.
    Faithfulness,
    /// The lesson against its summary.
    Relevance,
    /// Use of the documents it must cite.
    Coverage,
    /// Use of the sources' terms and notation.
    Terminology,
}

impl Criterion {
    pub const ALL: [Criterion; 4] = [
        Criterion::Faithfulness,
        Criterion::Relevance,
        Criterion::Coverage,
        Criterion::Terminology,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Criterion::Faithfulness => "faithfulness",
            Criterion::Relevance => "relevance",
            Criterion::Coverage => "coverage",
            Criterion::Terminology => "terminology",
        }
    }
}

/// A lesson or answer to judge, with what it was written from.
#[derive(Debug, Clone, PartialEq)]
pub struct LessonCase {
    pub id: String,
    pub title: String,
    pub summary: String,
    pub level: Level,
    pub body: String,
    /// The passages sent with it, numbered `[^1]` … in this order.
    pub passages: Vec<SentPassage>,
    pub must_cite: Vec<String>,
}

/// One judge call's verdict.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    pub grade: u8,
    /// Faithfulness: the mean claim grade.
    pub item_mean: Option<f64>,
    pub items: usize,
    pub reason: String,
}

/// A quoted answer span of a question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Span {
    pub document: String,
    #[serde(default)]
    pub section_path: String,
    #[serde(default)]
    pub page: Option<u32>,
    pub quote: String,
}

/// A question to grade for seriousness.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionCase {
    pub id: String,
    pub question: String,
    pub answer: String,
    pub spans: Vec<Span>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SeriousnessGrade {
    pub grade: u8,
    /// The spans exist and answer the question.
    pub supported: bool,
    /// What the grader called it ("application", "recall", …).
    pub kind: String,
    pub reason: String,
    pub grader: String,
}

impl SeriousnessGrade {
    /// Kept in the golden set: graded at least 2 and supported by its spans.
    pub fn keeps(&self) -> bool {
        self.grade >= 2 && self.supported
    }
}

/// A section questions are generated from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub document: String,
    pub section_path: String,
    pub page: Option<u32>,
    pub text: String,
}

/// A generated question, with the generator's own checks.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GeneratedQuestion {
    pub question: String,
    /// "application", "comparison", "derivation", "contradiction", "multi_hop".
    pub kind: String,
    pub answer: String,
    pub spans: Vec<Span>,
    /// Checks it failed (a span not found, a layout question…); empty when usable.
    pub problems: Vec<String>,
    pub generator: String,
}

/// Who won a pairwise comparison.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Winner {
    A,
    B,
    Tie,
}

/// Both orders of a pairwise comparison combined: a winner only when both agree.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct PairResult {
    pub winner: Winner,
    pub consistent: bool,
    pub grade_a: f64,
    pub grade_b: f64,
}

/// A deliberately bad lesson: the judge must grade `criterion` at most `max_grade`.
#[derive(Debug, Clone, PartialEq)]
pub struct LessonCanaryCase {
    pub id: String,
    pub defect: String,
    pub criterion: Criterion,
    pub max_grade: u8,
    pub case: LessonCase,
}

/// A question of known seriousness: a trivial one must be rejected, a serious one kept.
#[derive(Debug, Clone, PartialEq)]
pub struct QuestionCanaryCase {
    pub id: String,
    pub trivial: bool,
    pub case: QuestionCase,
}

pub trait Judge: Send + Sync {
    /// "openai:gpt-6-luna": recorded with every grade.
    fn id(&self) -> String;
    fn grade<'a>(
        &'a self,
        criterion: Criterion,
        case: &'a LessonCase,
    ) -> BoxFuture<'a, anyhow::Result<Grade>>;
    fn seriousness<'a>(
        &'a self,
        q: &'a QuestionCase,
    ) -> BoxFuture<'a, anyhow::Result<SeriousnessGrade>>;
    /// Compares two bodies written for `case` (its body is ignored), both orders.
    fn pair<'a>(
        &'a self,
        criterion: Criterion,
        case: &'a LessonCase,
        a: &'a str,
        b: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<PairResult>>;
    /// Generates up to `count` serious questions over `sections` (several for
    /// multi-hop).
    fn synth<'a>(
        &'a self,
        sections: &'a [Section],
        count: u32,
    ) -> BoxFuture<'a, anyhow::Result<Vec<GeneratedQuestion>>>;
    fn lesson_canaries(&self) -> Vec<LessonCanaryCase>;
    fn question_canaries(&self) -> Vec<QuestionCanaryCase>;
}

/// `case` with its passages in the opposite order and every `[^n]` in the body
/// renumbered to match, so the second pass sees the same lesson differently laid out.
pub fn reversed(case: &LessonCase) -> LessonCase {
    let n = case.passages.len();
    let mut out = case.clone();
    out.passages.reverse();
    let mut body = String::with_capacity(case.body.len());
    let mut rest = case.body.as_str();
    while let Some(at) = rest.find("[^") {
        body.push_str(&rest[..at]);
        let after = &rest[at + 2..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        let close = after[digits.len()..].starts_with(']');
        match digits.parse::<usize>() {
            Ok(k) if close && (1..=n).contains(&k) => {
                body.push_str(&format!("[^{}]", n + 1 - k));
                rest = &after[digits.len() + 1..];
            }
            _ => {
                body.push_str("[^");
                rest = after;
            }
        }
    }
    body.push_str(rest);
    out.body = body;
    out
}

/// Both passes over one lesson, per criterion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LessonJudgement {
    pub id: String,
    pub judge: String,
    /// Passages in the given order, then reversed.
    pub passes: [BTreeMap<Criterion, Grade>; 2],
}

impl LessonJudgement {
    /// The mean of both passes' grades on `c`.
    pub fn grade(&self, c: Criterion) -> f64 {
        self.passes
            .iter()
            .map(|p| p.get(&c).map_or(0.0, |g| f64::from(g.grade)))
            .sum::<f64>()
            / 2.0
    }

    /// Faithfulness as the mean claim grade over both passes (the lowest claim when a
    /// pass has no items).
    pub fn faithfulness_mean(&self) -> f64 {
        self.passes
            .iter()
            .map(|p| {
                p.get(&Criterion::Faithfulness)
                    .map_or(0.0, |g| g.item_mean.unwrap_or(f64::from(g.grade)))
            })
            .sum::<f64>()
            / 2.0
    }
}

/// Judges `case` on every criterion, twice.
pub async fn judge_lesson(judge: &dyn Judge, case: &LessonCase) -> anyhow::Result<LessonJudgement> {
    let back = reversed(case);
    let mut passes: [BTreeMap<Criterion, Grade>; 2] = Default::default();
    for (i, c) in [case, &back].into_iter().enumerate() {
        for criterion in Criterion::ALL {
            let g = judge
                .grade(criterion, c)
                .await
                .map_err(|e| e.context(format!("judging {} on {}", case.id, criterion.as_str())))?;
            passes[i].insert(criterion, g);
        }
    }
    Ok(LessonJudgement {
        id: case.id.clone(),
        judge: judge.id(),
        passes,
    })
}

/// Quadratic-weighted κ per criterion.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Agreement {
    pub n: usize,
    pub kappa: BTreeMap<Criterion, f64>,
}

impl Agreement {
    pub fn min(&self) -> f64 {
        self.kappa.values().copied().fold(1.0, f64::min)
    }
}

fn kappa_of(a: &[&BTreeMap<Criterion, Grade>], b: &[&BTreeMap<Criterion, Grade>]) -> Agreement {
    let mut kappa = BTreeMap::new();
    for c in Criterion::ALL {
        let x: Vec<u8> = a.iter().map(|p| p.get(&c).map_or(0, |g| g.grade)).collect();
        let y: Vec<u8> = b.iter().map(|p| p.get(&c).map_or(0, |g| g.grade)).collect();
        kappa.insert(c, quadratic_kappa(&x, &y));
    }
    Agreement { n: a.len(), kappa }
}

/// Agreement between the two passes of each judgement (order sensitivity).
pub fn pass_agreement(js: &[LessonJudgement]) -> Agreement {
    let a: Vec<_> = js.iter().map(|j| &j.passes[0]).collect();
    let b: Vec<_> = js.iter().map(|j| &j.passes[1]).collect();
    kappa_of(&a, &b)
}

/// Agreement between two judges over the same lessons (first passes, paired by id).
pub fn judge_agreement(a: &[LessonJudgement], b: &[LessonJudgement]) -> Agreement {
    let by_id: BTreeMap<&str, &LessonJudgement> = b.iter().map(|j| (j.id.as_str(), j)).collect();
    let (mut x, mut y) = (Vec::new(), Vec::new());
    for j in a {
        if let Some(k) = by_id.get(j.id.as_str()) {
            x.push(&j.passes[0]);
            y.push(&k.passes[0]);
        }
    }
    kappa_of(&x, &y)
}

/// Agreement between two seriousness graders over the same questions.
pub fn grader_agreement(a: &[u8], b: &[u8]) -> f64 {
    quadratic_kappa(a, b)
}

/// How one canary came out.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryOutcome {
    pub id: String,
    pub passed: bool,
    pub detail: String,
}

/// A judge's canary results; any failure invalidates the run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CanaryReport {
    pub judge: String,
    /// "lessons" or "questions".
    pub set: String,
    pub outcomes: Vec<CanaryOutcome>,
}

impl CanaryReport {
    pub fn valid(&self) -> bool {
        !self.outcomes.is_empty() && self.outcomes.iter().all(|o| o.passed)
    }
    pub fn failures(&self) -> Vec<&CanaryOutcome> {
        self.outcomes.iter().filter(|o| !o.passed).collect()
    }
}

/// Runs the bad-lesson canaries through `judge`: each must be graded at most its
/// `max_grade` on its criterion in both passage orders.
pub async fn lesson_canaries(judge: &dyn Judge) -> anyhow::Result<CanaryReport> {
    let mut outcomes = Vec::new();
    for c in judge.lesson_canaries() {
        let back = reversed(&c.case);
        let g1 = judge.grade(c.criterion, &c.case).await?.grade;
        let g2 = judge.grade(c.criterion, &back).await?.grade;
        outcomes.push(CanaryOutcome {
            id: c.id.clone(),
            passed: g1 <= c.max_grade && g2 <= c.max_grade,
            detail: format!(
                "{} graded {g1} and {g2} (passages reversed); must be ≤ {} ({})",
                c.criterion.as_str(),
                c.max_grade,
                c.defect
            ),
        });
    }
    Ok(CanaryReport {
        judge: judge.id(),
        set: "lessons".into(),
        outcomes,
    })
}

/// Runs the question canaries through a seriousness grader: trivial ones rejected,
/// serious ones kept.
pub async fn question_canaries(grader: &dyn Judge) -> anyhow::Result<CanaryReport> {
    let mut outcomes = Vec::new();
    for c in grader.question_canaries() {
        let g = grader.seriousness(&c.case).await?;
        let passed = if c.trivial { !g.keeps() } else { g.keeps() };
        outcomes.push(CanaryOutcome {
            id: c.id.clone(),
            passed,
            detail: format!(
                "graded {} ({}, {}); a {} question must be {}",
                g.grade,
                g.kind,
                if g.supported {
                    "supported"
                } else {
                    "unsupported"
                },
                if c.trivial { "trivial" } else { "serious" },
                if c.trivial { "rejected" } else { "kept" }
            ),
        });
    }
    Ok(CanaryReport {
        judge: grader.id(),
        set: "questions".into(),
        outcomes,
    })
}

#[cfg(test)]
pub(crate) mod testing {
    //! A rule-based judge for tests: grades by markers planted in the text.

    use super::*;

    /// Grades `[bad:<criterion>]` in a body 0 on that criterion and everything else
    /// 3; a question containing "trivial" gets 1, others 3. Counts its calls. With
    /// `lenient` it grades everything 3; with `order_bias` the reversed pass of a lesson
    /// (its first passage from a document named "z…") loses a point on relevance.
    #[derive(Default)]
    pub struct RuleJudge {
        pub name: String,
        pub lenient: bool,
        pub order_bias: bool,
        pub calls: std::sync::Mutex<usize>,
    }

    impl RuleJudge {
        pub fn named(name: &str) -> Self {
            Self {
                name: name.into(),
                ..Default::default()
            }
        }
    }

    pub fn case(id: &str, body: &str, passages: &[(&str, &str)]) -> LessonCase {
        LessonCase {
            id: id.into(),
            title: "T".into(),
            summary: "S".into(),
            level: Level::Intermediate,
            body: body.into(),
            passages: passages
                .iter()
                .map(|(d, t)| SentPassage {
                    document: (*d).into(),
                    text: (*t).into(),
                })
                .collect(),
            must_cite: vec![],
        }
    }

    impl Judge for RuleJudge {
        fn id(&self) -> String {
            format!("fake:{}", self.name)
        }
        fn grade<'a>(
            &'a self,
            c: Criterion,
            case: &'a LessonCase,
        ) -> BoxFuture<'a, anyhow::Result<Grade>> {
            Box::pin(async move {
                *self.calls.lock().unwrap() += 1;
                let bad = !self.lenient && case.body.contains(&format!("[bad:{}]", c.as_str()));
                let mut grade = if bad { 0 } else { 3 };
                if self.order_bias
                    && c == Criterion::Relevance
                    && case
                        .passages
                        .first()
                        .is_some_and(|p| p.document.starts_with('z'))
                {
                    grade -= 1;
                }
                Ok(Grade {
                    grade,
                    item_mean: (c == Criterion::Faithfulness).then_some(f64::from(grade)),
                    items: 1,
                    reason: String::new(),
                })
            })
        }
        fn seriousness<'a>(
            &'a self,
            q: &'a QuestionCase,
        ) -> BoxFuture<'a, anyhow::Result<SeriousnessGrade>> {
            Box::pin(async move {
                *self.calls.lock().unwrap() += 1;
                let trivial = !self.lenient && q.question.contains("trivial");
                Ok(SeriousnessGrade {
                    grade: if trivial { 1 } else { 3 },
                    supported: true,
                    kind: if trivial { "recall" } else { "application" }.into(),
                    reason: String::new(),
                    grader: self.id(),
                })
            })
        }
        fn pair<'a>(
            &'a self,
            _c: Criterion,
            _case: &'a LessonCase,
            a: &'a str,
            b: &'a str,
        ) -> BoxFuture<'a, anyhow::Result<PairResult>> {
            Box::pin(async move {
                let (ga, gb) = (a.len() as f64, b.len() as f64);
                Ok(PairResult {
                    winner: if ga > gb {
                        Winner::A
                    } else if gb > ga {
                        Winner::B
                    } else {
                        Winner::Tie
                    },
                    consistent: true,
                    grade_a: ga.min(3.0),
                    grade_b: gb.min(3.0),
                })
            })
        }
        fn synth<'a>(
            &'a self,
            sections: &'a [Section],
            count: u32,
        ) -> BoxFuture<'a, anyhow::Result<Vec<GeneratedQuestion>>> {
            Box::pin(async move {
                Ok(sections
                    .iter()
                    .take(count as usize)
                    .map(|s| {
                        let quote: String = s
                            .text
                            .split_whitespace()
                            .take(8)
                            .collect::<Vec<_>>()
                            .join(" ");
                        GeneratedQuestion {
                            question: format!(
                                "How would you apply what {} says to a new case?",
                                s.section_path
                            ),
                            kind: "application".into(),
                            answer: quote.clone(),
                            spans: vec![Span {
                                document: s.document.clone(),
                                section_path: s.section_path.clone(),
                                page: s.page,
                                quote,
                            }],
                            problems: vec![],
                            generator: self.id(),
                        }
                    })
                    .collect())
            })
        }
        fn lesson_canaries(&self) -> Vec<LessonCanaryCase> {
            Criterion::ALL
                .iter()
                .map(|c| LessonCanaryCase {
                    id: format!("bad-{}", c.as_str()),
                    defect: "planted".into(),
                    criterion: *c,
                    max_grade: 1,
                    case: case(
                        "x",
                        &format!("Body [bad:{}].[^1]", c.as_str()),
                        &[("a", "p1"), ("zz", "p2")],
                    ),
                })
                .collect()
        }
        fn question_canaries(&self) -> Vec<QuestionCanaryCase> {
            ["a trivial lookup", "a serious application"]
                .iter()
                .enumerate()
                .map(|(i, q)| QuestionCanaryCase {
                    id: format!("q{i}"),
                    trivial: q.contains("trivial"),
                    case: QuestionCase {
                        id: format!("q{i}"),
                        question: (*q).into(),
                        answer: String::new(),
                        spans: vec![],
                    },
                })
                .collect()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn reversing_renumbers_markers() {
        let c = case(
            "x",
            "One.[^1] Two [^2][^3]. Bad [^9] and [^x] and [^2",
            &[("a", "1"), ("b", "2"), ("c", "3")],
        );
        let r = reversed(&c);
        assert_eq!(r.passages[0].document, "c");
        assert_eq!(r.body, "One.[^3] Two [^2][^1]. Bad [^9] and [^x] and [^2");
        assert_eq!(reversed(&r), c);
    }

    #[tokio::test]
    async fn two_passes_and_their_agreement() {
        let j = RuleJudge {
            order_bias: true,
            ..RuleJudge::named("a")
        };
        let cases = [
            case("1", "Fine.[^1]", &[("a", "x"), ("zz", "y")]),
            case(
                "2",
                "Wrong [bad:faithfulness].[^1]",
                &[("a", "x"), ("zz", "y")],
            ),
            case("3", "Fine.", &[("a", "x")]),
        ];
        let mut js = Vec::new();
        for c in &cases {
            js.push(judge_lesson(&j, c).await.unwrap());
        }
        assert_eq!(*j.calls.lock().unwrap(), 3 * 4 * 2);
        assert_eq!(js[1].grade(Criterion::Faithfulness), 0.0);
        assert_eq!(js[0].faithfulness_mean(), 3.0);
        // The biased judge marks relevance down when the passages are reversed.
        assert_eq!(js[0].grade(Criterion::Relevance), 2.5);
        let a = pass_agreement(&js);
        assert_eq!(a.kappa[&Criterion::Faithfulness], 1.0);
        assert!(a.kappa[&Criterion::Relevance] < 1.0);
        assert!(a.min() < 1.0);
        // Two judges: one lenient disagrees on the planted flaw.
        let lenient = RuleJudge {
            lenient: true,
            ..RuleJudge::named("b")
        };
        let mut ks = Vec::new();
        for c in &cases {
            ks.push(judge_lesson(&lenient, c).await.unwrap());
        }
        let ab = judge_agreement(&js, &ks);
        assert_eq!(ab.n, 3);
        assert!(ab.kappa[&Criterion::Faithfulness] < 1.0);
        assert_eq!(grader_agreement(&[0, 3], &[0, 3]), 1.0);
    }

    #[tokio::test]
    async fn canaries_validate_a_judge_and_catch_a_lenient_one() {
        let good = RuleJudge::named("good");
        let r = lesson_canaries(&good).await.unwrap();
        assert!(r.valid(), "{:?}", r.failures());
        assert_eq!(r.outcomes.len(), 4);
        let q = question_canaries(&good).await.unwrap();
        assert!(q.valid(), "{:?}", q.failures());
        let lenient = RuleJudge {
            lenient: true,
            ..RuleJudge::named("lenient")
        };
        let r = lesson_canaries(&lenient).await.unwrap();
        assert_eq!(r.failures().len(), 4);
        assert!(r.failures()[0].detail.contains("must be ≤ 1"));
        let q = question_canaries(&lenient).await.unwrap();
        assert_eq!(q.failures().len(), 1);
        assert!(q.failures()[0].detail.contains("must be rejected"));
        // No canaries at all is never valid.
        let empty = CanaryReport {
            judge: "j".into(),
            set: "lessons".into(),
            outcomes: vec![],
        };
        assert!(!empty.valid());
    }
}
