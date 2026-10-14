//! The courses suite (§3.1 `golden/courses.jsonl`, §3.2 generation metrics): every
//! lesson of every golden course retrieved for, packed to the variant's budget,
//! written with the variant's prompt set, recorded (`evals/recorded/generations/`),
//! then checked deterministically (citations, structure, must-cite) and, in the runner,
//! judged. Also the prompt-prefix gate (§2.6, §3.3): the constant part of a prompt
//! version is byte-identical across every golden lesson's call.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::Context;
use margin_ai::{LessonContext, Passage};
use margin_service::pipeline::{PackBudget, Packed, Prompt, PromptSet};
use margin_shared::context::{Scope, TraceUsage};
use margin_shared::{Level, OutlineLesson};
use serde::{Deserialize, Serialize};

use crate::citations::{CitationReport, SentPassage, check, check_structure};
use crate::golden::CourseRequest;
use crate::index::EvalIndex;
use crate::judge::{Judge, LessonCase, LessonJudgement, judge_lesson};
use crate::model::{EvalModel, ModelRequest};

/// `evals/recorded/generations/`.
pub const GENERATIONS_DIR: &str = "recorded/generations";

/// The outline of a golden course: its themes, in order, as lessons.
pub fn outline(course: &CourseRequest) -> Vec<OutlineLesson> {
    course
        .themes
        .iter()
        .map(|t| OutlineLesson {
            title: t.clone(),
            summary: format!("{t}, as the attached sources treat it."),
            minutes: 10,
        })
        .collect()
}

/// Level and quiz vary across the golden lessons so the prefix gate sees every
/// variable part change.
fn level_of(i: usize) -> Level {
    [Level::Intro, Level::Intermediate, Level::Advanced][i % 3]
}

/// The context of lesson `index` of `course` with `passages`.
pub fn lesson_context(course: &CourseRequest, index: usize, passages: Vec<Passage>) -> LessonContext {
    let outline = outline(course);
    LessonContext {
        subject_title: course.themes.first().cloned().unwrap_or_default(),
        topic_title: course.prompt.chars().take(60).collect(),
        prompt: course.prompt.clone(),
        level: level_of(index),
        with_quiz: index % 2 == 0,
        outline,
        index,
        passages,
    }
}

/// The retrieval plan of a lesson: title and summary, the summary, and the learner's
/// prompt for the first lesson (§2.3 step 1, without the planner's sub-questions).
pub fn query_plan(course: &CourseRequest, index: usize) -> Vec<String> {
    let l = &outline(course)[index];
    let mut plan = vec![format!("{} {}", l.title, l.summary), l.title.clone()];
    if index == 0 {
        plan.push(course.prompt.clone());
    }
    plan
}

/// A packed passage as the lesson writer gets it (pictures of image files attached).
pub fn to_passage(corpus: &Path, p: &Packed) -> Passage {
    let first = &p.hits[0].passage;
    let image = (first.modality == "picture" && margin_shared::SourceKind::for_name(&first.document_id)
        == Some(margin_shared::SourceKind::Image))
    .then(|| std::fs::read(corpus.join(&first.document_id)).ok())
    .flatten()
    .map(|data| margin_ai::Image {
        media_type: "image/png".into(),
        data,
    });
    Passage {
        source: first.document_id.clone(),
        page: first.page.map(|v| v as u32),
        text: p.text.clone(),
        start_ms: None,
        end_ms: None,
        image,
    }
}

/// Every prompt-prefix rule a set of prompts of one version breaks: the constant part
/// differs between calls, the cache key differs, the constant is under ~1 024 tokens
/// (too short to cache), or a call's own strings (`unique[i]`: its lesson title, …)
/// leak into the constant part.
pub fn check_prefixes(prompts: &[Prompt], unique: &[Vec<String>]) -> Vec<String> {
    let mut out = Vec::new();
    let Some(first) = prompts.first() else {
        return out;
    };
    for (i, p) in prompts.iter().enumerate() {
        if p.version != first.version {
            out.push(format!("call {i} is version {}, not {}", p.version, first.version));
        }
        if p.constant != first.constant {
            let at = p
                .constant
                .chars()
                .zip(first.constant.chars())
                .position(|(a, b)| a != b)
                .unwrap_or(p.constant.len().min(first.constant.len()));
            out.push(format!(
                "call {i}'s constant part differs from call 0's at character {at}"
            ));
        }
        if p.cache_key != first.cache_key {
            out.push(format!(
                "call {i}'s cache key {} isn't {}",
                p.cache_key, first.cache_key
            ));
        }
        for s in unique.get(i).into_iter().flatten() {
            if !s.is_empty() && p.constant.contains(s.as_str()) {
                out.push(format!("call {i}'s own text \"{s}\" is in the constant part"));
            }
        }
    }
    let tokens = crate::corpus::approx_tokens(&first.constant);
    if tokens < 1_024 {
        out.push(format!(
            "the constant part is about {tokens} tokens, under the 1 024 a provider caches"
        ));
    }
    out
}

/// The prompts of every golden lesson under `prompts`, with each call's own strings.
pub fn golden_prompts(
    prompts: &dyn PromptSet,
    courses: &[CourseRequest],
) -> (Vec<Prompt>, Vec<Vec<String>>) {
    let mut out = (Vec::new(), Vec::new());
    for c in courses {
        for i in 0..c.themes.len() {
            let ctx = lesson_context(
                c,
                i,
                vec![Passage {
                    source: c.documents[0].clone(),
                    text: format!("A passage for {}.", c.themes[i]),
                    ..Default::default()
                }],
            );
            out.0.push(prompts.lesson(&ctx));
            out.1.push(vec![c.themes[i].clone(), c.prompt.clone()]);
        }
    }
    out
}

/// One generated lesson, as recorded.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Generation {
    pub course: String,
    pub lesson: usize,
    pub title: String,
    pub summary: String,
    pub level: Level,
    pub with_quiz: bool,
    pub prompt_version: String,
    pub model: String,
    /// The passages sent, in `[^n]` order.
    pub passages: Vec<SentPassage>,
    /// Their passage ids (first hit of each).
    pub packed_ids: Vec<String>,
    pub packed_tokens: usize,
    pub must_cite: Vec<String>,
    pub body: String,
    pub usage: TraceUsage,
}

impl Generation {
    pub fn case(&self) -> LessonCase {
        LessonCase {
            id: format!("{}-{}", self.course, self.lesson + 1),
            title: self.title.clone(),
            summary: self.summary.clone(),
            level: self.level,
            body: self.body.clone(),
            passages: self.passages.clone(),
            must_cite: self.must_cite.clone(),
        }
    }

    pub fn citations(&self) -> CitationReport {
        check(&self.body, &self.passages, &self.must_cite)
    }

    pub fn structure(&self) -> Vec<String> {
        check_structure(&self.body, self.with_quiz)
    }

    fn file_name(&self) -> String {
        format!("{}-{:02}.json", self.course, self.lesson + 1)
    }
}

/// `generations/<prompt_version>/<model>/` under `evals_dir`.
pub fn generations_dir(evals_dir: &Path, prompt_version: &str, model: &str) -> PathBuf {
    evals_dir
        .join(GENERATIONS_DIR)
        .join(prompt_version)
        .join(model.replace([':', '/'], "_"))
}

pub fn save_generation(dir: &Path, g: &Generation) -> anyhow::Result<()> {
    std::fs::create_dir_all(dir)?;
    std::fs::write(dir.join(g.file_name()), serde_json::to_string_pretty(g)? + "\n")
        .with_context(|| format!("couldn't write {}", g.file_name()))
}

/// Every recorded generation under `dir` (recursively), in path order.
pub fn load_generations(dir: &Path) -> anyhow::Result<Vec<Generation>> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        if !dir.is_dir() {
            return Ok(());
        }
        for e in std::fs::read_dir(dir)? {
            let p = e?.path();
            if p.is_dir() {
                walk(&p, out)?;
            } else if p.extension().is_some_and(|x| x == "json") {
                out.push(p);
            }
        }
        Ok(())
    }
    let mut paths = Vec::new();
    walk(dir, &mut paths)?;
    paths.sort();
    paths
        .iter()
        .map(|p| {
            let text = std::fs::read_to_string(p)?;
            serde_json::from_str(&text).with_context(|| format!("bad {}", p.display()))
        })
        .collect()
}

/// Retrieves, packs and writes one lesson.
pub async fn generate_lesson(
    index: &EvalIndex<'_>,
    writer: &dyn EvalModel,
    course: &CourseRequest,
    lesson: usize,
    budget: &PackBudget,
) -> anyhow::Result<Generation> {
    let scope = Scope::Sources {
        ids: course.documents.clone(),
    };
    let ranked = index.search(&query_plan(course, lesson), &scope).await?;
    let packed = index.pack(ranked.hits, budget);
    let passages: Vec<Passage> = packed.iter().map(|p| to_passage(&index.corpus, p)).collect();
    let ctx = lesson_context(course, lesson, passages);
    let prompt = index.pipeline.prompts.lesson(&ctx);
    let mut req = ModelRequest::new(prompt.constant.clone(), prompt.variable.clone());
    req.cache_key = Some(prompt.cache_key.clone());
    req.images = ctx.passages.iter().filter_map(|p| p.image.clone()).collect();
    req.max_output_tokens = 8_000;
    let reply = writer.complete(&req).await?;
    let l = &ctx.outline[lesson];
    Ok(Generation {
        course: course.id.clone(),
        lesson,
        title: l.title.clone(),
        summary: l.summary.clone(),
        level: ctx.level,
        with_quiz: ctx.with_quiz,
        prompt_version: prompt.version.clone(),
        model: writer.id(),
        passages: packed
            .iter()
            .map(|p| SentPassage {
                document: p.hits[0].passage.document_id.clone(),
                text: p.text.clone(),
            })
            .collect(),
        packed_ids: packed.iter().map(|p| p.hits[0].passage.id.clone()).collect(),
        packed_tokens: packed.iter().map(|p| p.tokens).sum(),
        must_cite: course.must_cite.get(lesson).cloned().unwrap_or_default(),
        body: reply.text,
        usage: reply.usage,
    })
}

/// The per-lesson numbers the suite reports and stores.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LessonScore {
    pub id: String,
    pub course: String,
    pub citations: CitationReport,
    pub structure_problems: Vec<String>,
    pub packed_tokens: usize,
    pub usage: TraceUsage,
    pub judgements: Vec<LessonJudgement>,
}

impl LessonScore {
    /// Metric name → value, for `eval_result` rows.
    pub fn metrics(&self) -> BTreeMap<String, f64> {
        let mut m = BTreeMap::new();
        m.insert("citation_resolution".into(), self.citations.resolution_pct() / 100.0);
        m.insert("citation_support".into(), self.citations.lexical_support_pct() / 100.0);
        m.insert("cited_sentences".into(), self.citations.cited_sentence_pct() / 100.0);
        m.insert("must_cite".into(), self.citations.must_cite_pct() / 100.0);
        m.insert("structure_ok".into(), f64::from(u8::from(self.structure_problems.is_empty())));
        m.insert("packed_tokens".into(), self.packed_tokens as f64);
        m.insert("input_tokens".into(), self.usage.input as f64);
        m.insert("output_tokens".into(), self.usage.output as f64);
        for j in &self.judgements {
            let who = if self.judgements.len() > 1 {
                format!("@{}", j.judge)
            } else {
                String::new()
            };
            m.insert(format!("faithfulness{who}"), j.faithfulness_mean() / 3.0);
            for c in crate::judge::Criterion::ALL {
                m.insert(format!("{}_grade{who}", c.as_str()), j.grade(c) / 3.0);
            }
        }
        m
    }
}

/// Checks (and, with judges, judges) recorded generations.
pub async fn score(gens: &[Generation], judges: &[&dyn Judge]) -> anyhow::Result<Vec<LessonScore>> {
    let mut out = Vec::new();
    for g in gens {
        let mut judgements = Vec::new();
        for j in judges {
            judgements.push(judge_lesson(*j, &g.case()).await?);
        }
        out.push(LessonScore {
            id: g.case().id,
            course: g.course.clone(),
            citations: g.citations(),
            structure_problems: g.structure(),
            packed_tokens: g.packed_tokens,
            usage: g.usage,
            judgements,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::{evals_dir, read_jsonl};

    fn courses() -> Vec<CourseRequest> {
        read_jsonl(&evals_dir().join("golden/courses.jsonl")).unwrap()
    }

    fn prompt(constant: &str, variable: &str) -> Prompt {
        Prompt {
            version: "lesson-v2".into(),
            constant: constant.into(),
            variable: variable.into(),
            cache_key: "lesson:lesson-v2".into(),
        }
    }

    #[test]
    fn the_prefix_gate() {
        let long = "Write a lesson. ".repeat(300);
        let ok = vec![prompt(&long, "Level: intro"), prompt(&long, "Level: advanced")];
        assert!(check_prefixes(&ok, &[vec!["Slow start".into()], vec![]]).is_empty());
        let mut bad = ok.clone();
        bad[1].constant = format!("{long}Level: advanced");
        bad[1].cache_key = "lesson:other".into();
        let v = check_prefixes(&bad, &[vec![], vec!["Level: advanced".into()]]).join("\n");
        assert!(v.contains("differs from call 0's at character"), "{v}");
        assert!(v.contains("cache key"), "{v}");
        assert!(v.contains("is in the constant part"), "{v}");
        let short = vec![prompt("Write a lesson.", "")];
        assert!(check_prefixes(&short, &[])[0].contains("under the 1 024"));
    }

    #[test]
    fn golden_lessons_vary_their_variable_parts() {
        let cs = courses();
        let c = &cs[0];
        let a = lesson_context(c, 0, vec![]);
        let b = lesson_context(c, 1, vec![]);
        assert_ne!((a.level, a.with_quiz), (b.level, b.with_quiz));
        assert_eq!(a.outline.len(), c.themes.len());
        assert_eq!(query_plan(c, 0).len(), 3);
        assert_eq!(query_plan(c, 1).len(), 2);
        // A fake prompt set that is constant-first passes; one that leaks the title fails.
        struct Good;
        impl PromptSet for Good {
            fn version(&self) -> &'static str {
                "lesson-v2"
            }
            fn lesson(&self, ctx: &LessonContext) -> Prompt {
                prompt(&"Rules. ".repeat(800), &ctx.outline[ctx.index].title)
            }
            fn plan(&self, _: &margin_service::pipeline::PlanInput) -> Prompt {
                unreachable!()
            }
            fn rerank(&self, _: &str, _: &[margin_service::pipeline::Hit]) -> Prompt {
                unreachable!()
            }
        }
        let (ps, uniq) = golden_prompts(&Good, &cs);
        assert_eq!(ps.len(), cs.iter().map(|c| c.themes.len()).sum::<usize>());
        assert!(check_prefixes(&ps, &uniq).is_empty());
    }

    #[test]
    fn generations_round_trip_and_score() {
        let g = Generation {
            course: "c01".into(),
            lesson: 3,
            title: "Convergence".into(),
            summary: "s".into(),
            level: Level::Intro,
            with_quiz: false,
            prompt_version: "lesson-v2".into(),
            model: "openai:x".into(),
            passages: vec![SentPassage {
                document: "fourier-series.md".into(),
                text: "Adding more terms does not shrink the overshoot near the jump.".into(),
            }],
            packed_ids: vec!["fourier-series.md|t3".into()],
            packed_tokens: 20,
            must_cite: vec!["fourier-series.md".into()],
            body: "Why.\n\n## Worked example\n\nMore terms do not shrink the overshoot near a jump.[^1]\n\n> **Key idea.** It stays.\n".into(),
            usage: TraceUsage::default(),
        };
        let dir = std::env::temp_dir().join(format!("margin-evals-gen-{}", std::process::id()));
        let d = generations_dir(&dir, "lesson-v2", "openai:x");
        assert!(d.ends_with("recorded/generations/lesson-v2/openai_x"));
        save_generation(&d, &g).unwrap();
        assert!(d.join("c01-04.json").is_file());
        let back = load_generations(&dir).unwrap();
        assert_eq!(back, vec![g.clone()]);
        assert!(load_generations(&dir.join("missing")).unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
        let c = g.citations();
        assert_eq!((c.resolution_pct(), c.must_cite_pct()), (100.0, 100.0));
        assert!(g.structure().is_empty(), "{:?}", g.structure());
    }

    #[tokio::test]
    async fn scoring_with_a_judge() {
        let g = Generation {
            course: "c".into(),
            lesson: 0,
            title: "t".into(),
            summary: "s".into(),
            level: Level::Intro,
            with_quiz: true,
            prompt_version: "v".into(),
            model: "m".into(),
            passages: vec![],
            packed_ids: vec![],
            packed_tokens: 0,
            must_cite: vec![],
            body: "Text [bad:relevance].[^4]".into(),
            usage: TraceUsage::default(),
        };
        let j = crate::judge::testing::RuleJudge::named("j");
        let s = score(std::slice::from_ref(&g), &[&j]).await.unwrap();
        let m = s[0].metrics();
        assert_eq!(m["citation_resolution"], 0.0);
        assert_eq!(m["structure_ok"], 0.0);
        assert_eq!(m["relevance_grade"], 0.0);
        assert_eq!(m["faithfulness"], 1.0);
    }
}
