//! The real [`Judge`]: R1-ai's eval prompts (`margin_ai::evals`, `synth-v1`,
//! `seriousness-v1`, `judge-v1`, `pair-v1`) and canary sets, sent through an
//! [`EvalModel`] so either provider can judge (OpenAI with the prompt's strict JSON
//! schema, Claude with a cache breakpoint after the constant block).
//!
//! Built with the `r1-ai` feature, which integration turns on (and then removes) when
//! R1-ai's branch is merged: this file is written against its API (`agent/r1-ai`,
//! commit 10c3a6a) and doesn't compile without it.

use std::sync::Arc;

use margin_ai::evals::{self, AnswerSpan, QuestionToGrade, Rubric, SynthRequest, SynthSection};
use margin_ai::{InputPart, Passage, Prompt};
use margin_service::engines::BoxFuture;

use crate::citations::SentPassage;
use crate::judge::{
    Criterion, GeneratedQuestion, Grade, Judge, LessonCanaryCase, LessonCase, PairResult,
    QuestionCanaryCase, QuestionCase, Section, SeriousnessGrade, Span, Winner,
};
use crate::model::{EvalModel, ModelRequest, parse_json};

pub struct AiJudge {
    pub model: Arc<dyn EvalModel>,
}

fn rubric(c: Criterion) -> Rubric {
    match c {
        Criterion::Faithfulness => Rubric::Faithfulness,
        Criterion::Relevance => Rubric::Relevance,
        Criterion::Coverage => Rubric::Coverage,
        Criterion::Terminology => Rubric::Terminology,
    }
}

fn criterion(r: Rubric) -> Criterion {
    match r {
        Rubric::Faithfulness => Criterion::Faithfulness,
        Rubric::Relevance => Criterion::Relevance,
        Rubric::Coverage => Criterion::Coverage,
        Rubric::Terminology => Criterion::Terminology,
    }
}

/// A prompt as one model request.
pub fn request(p: &Prompt) -> ModelRequest {
    let mut text = Vec::new();
    let mut images = Vec::new();
    for part in &p.input {
        match part {
            InputPart::Text(t) => text.push(t.clone()),
            InputPart::Image(i) => images.push(i.clone()),
        }
    }
    let mut req = ModelRequest::new(p.instructions(), text.join("\n\n"));
    req.images = images;
    req.cache_key = Some(p.cache_key());
    req.format = p
        .format
        .as_ref()
        .map(|f| (f.name.to_string(), f.schema.clone()));
    req
}

fn input(case: &LessonCase) -> evals::JudgeInput {
    evals::JudgeInput {
        title: case.title.clone(),
        summary: case.summary.clone(),
        level: case.level,
        passages: case
            .passages
            .iter()
            .map(|p| Passage {
                source: p.document.clone(),
                text: p.text.clone(),
                ..Default::default()
            })
            .collect(),
        must_cite: case.must_cite.clone(),
    }
}

fn span(s: &Span) -> AnswerSpan {
    AnswerSpan {
        document: s.document.clone(),
        section_path: s.section_path.clone(),
        page: s.page,
        quote: s.quote.clone(),
    }
}

fn from_span(s: &AnswerSpan) -> Span {
    Span {
        document: s.document.clone(),
        section_path: s.section_path.clone(),
        page: s.page,
        quote: s.quote.clone(),
    }
}

fn name_of<T: serde::Serialize>(v: &T) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl AiJudge {
    async fn ask<T: serde::de::DeserializeOwned>(&self, p: &Prompt) -> anyhow::Result<T> {
        let reply = self.model.complete(&request(p)).await?;
        parse_json(&reply.text)
    }
}

impl Judge for AiJudge {
    fn id(&self) -> String {
        self.model.id()
    }

    fn grade<'a>(
        &'a self,
        c: Criterion,
        case: &'a LessonCase,
    ) -> BoxFuture<'a, anyhow::Result<Grade>> {
        Box::pin(async move {
            let v: evals::Verdict = self
                .ask(&evals::judge(rubric(c), &input(case), &case.body))
                .await?;
            Ok(Grade {
                grade: v.grade.min(3),
                item_mean: v.item_mean().map(f64::from),
                items: v.items.len(),
                reason: v.reason,
            })
        })
    }

    fn seriousness<'a>(
        &'a self,
        q: &'a QuestionCase,
    ) -> BoxFuture<'a, anyhow::Result<SeriousnessGrade>> {
        Box::pin(async move {
            let g: evals::Seriousness = self
                .ask(&evals::seriousness(&QuestionToGrade {
                    question: q.question.clone(),
                    answer: q.answer.clone(),
                    spans: q.spans.iter().map(span).collect(),
                }))
                .await?;
            Ok(SeriousnessGrade {
                grade: g.grade.min(3),
                supported: g.supported,
                kind: name_of(&g.kind),
                reason: g.reason,
                grader: self.model.id(),
            })
        })
    }

    fn pair<'a>(
        &'a self,
        c: Criterion,
        case: &'a LessonCase,
        a: &'a str,
        b: &'a str,
    ) -> BoxFuture<'a, anyhow::Result<PairResult>> {
        Box::pin(async move {
            let i = input(case);
            let ab: evals::PairVerdict = self.ask(&evals::pair(rubric(c), &i, a, b)).await?;
            let ba: evals::PairVerdict = self.ask(&evals::pair(rubric(c), &i, b, a)).await?;
            let o = evals::combine_pair(&ab, &ba);
            Ok(PairResult {
                winner: match o.winner {
                    evals::Winner::A => Winner::A,
                    evals::Winner::B => Winner::B,
                    evals::Winner::Tie => Winner::Tie,
                },
                consistent: o.consistent,
                grade_a: f64::from(o.grade_a),
                grade_b: f64::from(o.grade_b),
            })
        })
    }

    fn synth<'a>(
        &'a self,
        sections: &'a [Section],
        count: u32,
    ) -> BoxFuture<'a, anyhow::Result<Vec<GeneratedQuestion>>> {
        Box::pin(async move {
            let req = SynthRequest {
                sections: sections
                    .iter()
                    .map(|s| SynthSection {
                        document: s.document.clone(),
                        section_path: s.section_path.clone(),
                        page: s.page,
                        text: s.text.clone(),
                    })
                    .collect(),
                count,
                kinds: Vec::new(),
            };
            let raw: evals::RawSynth = self.ask(&evals::synth(&req)).await?;
            Ok(evals::check_synth(raw, &req)
                .into_iter()
                .map(|q| GeneratedQuestion {
                    kind: name_of(&q.kind),
                    spans: q.spans.iter().map(from_span).collect(),
                    question: q.question,
                    answer: q.answer,
                    problems: q.problems,
                    generator: self.model.id(),
                })
                .collect())
        })
    }

    fn lesson_canaries(&self) -> Vec<LessonCanaryCase> {
        evals::BAD_LESSONS
            .iter()
            .map(|c| {
                let i = c.input();
                LessonCanaryCase {
                    id: c.id.to_string(),
                    defect: format!("{:?}", c.defect),
                    criterion: criterion(c.rubric),
                    max_grade: c.max_grade,
                    case: LessonCase {
                        id: c.id.to_string(),
                        title: i.title,
                        summary: i.summary,
                        level: i.level,
                        body: c.body.to_string(),
                        passages: i
                            .passages
                            .into_iter()
                            .map(|p| SentPassage {
                                document: p.source,
                                text: p.text,
                            })
                            .collect(),
                        must_cite: i.must_cite,
                    },
                }
            })
            .collect()
    }

    fn question_canaries(&self) -> Vec<QuestionCanaryCase> {
        let to = |c: &evals::QuestionCanary, trivial: bool| {
            let g = c.to_grade();
            QuestionCanaryCase {
                id: c.id.to_string(),
                trivial,
                case: QuestionCase {
                    id: c.id.to_string(),
                    question: g.question,
                    answer: g.answer,
                    spans: g.spans.iter().map(from_span).collect(),
                },
            }
        };
        evals::TRIVIAL_QUESTIONS
            .iter()
            .map(|c| to(c, true))
            .chain(evals::SERIOUS_QUESTIONS.iter().map(|c| to(c, false)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::judge::testing::case;
    use crate::model::ScriptedModel;

    #[tokio::test]
    async fn r1_ai_prompts_through_an_eval_model() {
        let model = Arc::new(ScriptedModel::new("anthropic", "j", |req| {
            assert!(req.cache_key.is_some());
            assert!(!req.system.is_empty());
            Ok(if req.format.as_ref().is_some_and(|f| f.0 == "verdict") {
                r#"{"grade": 1, "items": [{"quote": "q", "passages": [1], "grade": 1, "note": ""}], "reason": "r"}"#.into()
            } else {
                r#"{"grade": 3, "supported": true, "kind": "application", "reason": "r"}"#.into()
            })
        }));
        let j = AiJudge {
            model: model.clone(),
        };
        assert_eq!(j.lesson_canaries().len(), 20);
        let qs = j.question_canaries();
        assert_eq!(qs.iter().filter(|q| q.trivial).count(), 10);
        assert_eq!(qs.len(), 15);
        let g = j
            .grade(
                Criterion::Faithfulness,
                &case("x", "Body.[^1]", &[("a.md", "text")]),
            )
            .await
            .unwrap();
        assert_eq!((g.grade, g.items), (1, 1));
        let s = j.seriousness(&qs[0].case).await.unwrap();
        assert!(s.keeps());
        assert_eq!(s.kind, "application");
    }
}
