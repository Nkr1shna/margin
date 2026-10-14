//! The instructions and inputs Margin sends to the model. These are the product: the
//! app renders exactly what they ask for.

use std::fmt::Write as _;

use margin_shared::{AskRequest, Level, OutlineLesson};
use serde_json::{Value, json};

use crate::{LessonContext, SceneRequest};

fn level_words(level: Level) -> &'static str {
    match level {
        Level::Intro => {
            "introductory: assume only school mathematics and no prior exposure to the topic; \
             build intuition with concrete pictures and small numbers before any formalism"
        }
        Level::Intermediate => {
            "intermediate: assume an undergraduate who knows the prerequisites but not this \
             topic; balance intuition with the standard definitions, notation and derivations"
        }
        Level::Advanced => {
            "advanced: assume a strong student who wants depth; be precise, state results \
             carefully, prove or derive the key ones and point out subtleties and edge cases"
        }
    }
}

// ---------------------------------------------------------------------------
// Outline
// ---------------------------------------------------------------------------

pub const OUTLINE_SCHEMA_NAME: &str = "course_outline";

/// Strict-mode JSON schema for `margin_shared::Outline`. Strict mode requires every
/// property to be listed in `required` and `additionalProperties: false`.
pub fn outline_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["topic_title", "subject_title", "lessons"],
        "properties": {
            "topic_title": {
                "type": "string",
                "description": "Short title of the topic the learner asked about, e.g. \"Fourier series\"."
            },
            "subject_title": {
                "type": "string",
                "description": "The broad subject to file the topic under, e.g. \"Signals & Systems\"."
            },
            "lessons": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "required": ["title", "summary", "minutes"],
                    "properties": {
                        "title": { "type": "string", "description": "Lesson title, at most about 60 characters." },
                        "summary": { "type": "string", "description": "One line saying what the lesson covers." },
                        "minutes": { "type": "integer", "description": "Realistic reading time in minutes." }
                    }
                }
            }
        }
    })
}

pub fn outline_instructions(lesson_count: u32, level: Level) -> String {
    format!(
        "You design short self-study courses for Margin, a personal study app. The learner \
describes what they want to learn; you propose an outline that the app turns into lessons \
written by another step.

Answer with the JSON object the schema describes:
- `topic_title`: a short, specific title for the topic (2 to 6 words, title case, no \
trailing punctuation), e.g. \"Fourier series\". Name the topic, not the course: no \
\"Introduction to\", \"A course on\" or level words.
- `subject_title`: the broad subject the topic belongs to, e.g. \"Signals & Systems\", \
\"Linear Algebra\", \"Organic Chemistry\". If one of the learner's existing subjects \
fits, copy its title exactly; otherwise invent a new, equally broad subject title.
- `lessons`: exactly {lesson_count} lesson{s}, no more and no fewer.

The lessons:
- Level: {level}.
- Order them from intuition to depth: start from the motivating question and a \
concrete picture, then the core definitions and mechanics, then worked techniques, then \
deeper results, connections and applications. Each lesson builds only on earlier ones.
- Each `title` is short and concrete (at most about 60 characters), says what the \
lesson is about, and does not repeat the topic title or carry a number such as \
\"Lesson 1\" or \"Part 2\".
- Each `summary` is one line (one sentence, at most about 140 characters, no line \
breaks) saying what the learner will understand or be able to do after it.
- Each `minutes` is a realistic reading time for a focused written lesson with worked \
examples at this level: usually 6 to 20, more for dense derivations. Never 0.
- Cover what the learner asked for. If they asked for something narrow, go deeper rather \
than padding with loosely related material; if broad, choose the essentials.

Write in the learner's language. Use plain text in every field: no Markdown, no LaTeX, \
no emoji.",
        lesson_count = lesson_count,
        s = if lesson_count == 1 { "" } else { "s" },
        level = level_words(level),
    )
}

pub fn outline_input(req: &AskRequest, existing_subjects: &[String]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "What I want to learn:\n{}\n", req.prompt.trim());
    let _ = writeln!(s, "Number of lessons: {}", req.lesson_count);
    let _ = writeln!(s, "Level: {}", level_name(req.level));
    if existing_subjects.is_empty() {
        let _ = writeln!(s, "My existing subjects: none yet.");
    } else {
        let _ = writeln!(s, "My existing subjects:");
        for subject in existing_subjects {
            let _ = writeln!(s, "- {subject}");
        }
    }
    s
}

fn level_name(level: Level) -> &'static str {
    match level {
        Level::Intro => "introductory",
        Level::Intermediate => "intermediate",
        Level::Advanced => "advanced",
    }
}

// ---------------------------------------------------------------------------
// Lesson
// ---------------------------------------------------------------------------

pub fn lesson_instructions(ctx: &LessonContext) -> String {
    let mut s = format!(
        "You write one lesson of a self-study course in Margin, a personal study app. \
Answer with the lesson body only, as Markdown. Another step wrote the outline; other \
lessons of the course are written separately, so teach only this lesson's material and \
refer to the others where it helps.

Level: {level}.

How the app renders your answer, which you must follow exactly:
- Do not start with a title: the app shows the lesson title above your text. Never use \
a `# ` heading. Start with one or two short paragraphs that say why this lesson matters \
and what question it answers.
- Organise the lesson with `## ` section headings (sentence case, short). Use `### ` \
sparingly for subsections. Aim for 3 to 6 sections.
- Short paragraphs: two to four sentences each. Prefer clear prose to long bullet lists; \
use a list only for genuinely list-like content.
- Mathematics uses KaTeX syntax. Inline math goes between single dollar signs: \
`$f(x) = x^2$`. Display math goes between double dollar signs, each `$$` on its own line, \
with a blank line before and after:

$$
\\int_0^1 x^2 \\, dx = \\frac{{1}}{{3}}
$$

  Use only commands KaTeX supports (no `\\begin{{align}}` outside display math, no \
`\\label`, no custom macros). Never use `\\(`, `\\)`, `\\[`, `\\]`. Write a literal dollar \
sign as `\\$`.
- Include at least one fully worked example, step by step, with concrete numbers or a \
concrete case, under its own `## ` heading (e.g. \"## Worked example\"). Show every \
step a learner at this level would need.
- Include exactly one key-idea callout, a blockquote that starts with the bold words \
`Key idea.`, stating the single most important takeaway in one or two sentences:

> **Key idea.** The derivative is the slope of the tangent line, the limit of slopes of \
secant lines.

- Use code blocks only for actual code the learner should read, with a language tag. \
Use tables only when comparing things side by side. No images, no HTML except where \
asked below, no emoji.
- Length: aim for what a learner reads in about {minutes} minutes (roughly {words} words), \
including the examples.
- Do not wrap your answer in a code fence. Do not add a closing summary of the course, \
sign-offs, or questions to the reader outside the section described below.",
        level = level_words(ctx.level),
        minutes = lesson_minutes(ctx),
        // Math-dense text reads slower than prose: about 150 words a minute.
        words = lesson_minutes(ctx) * 150,
    );
    if ctx.with_quiz {
        s.push_str(
            "

End the lesson with a section headed exactly `## Check yourself` containing 2 to 4 \
questions that test understanding of this lesson (not recall of trivia). Write each \
question as raw HTML, exactly in this form, with a blank line before and after each one:

<details><summary>What is the derivative of $x^3$?</summary>

By the power rule it is $3x^2$: bring the exponent down and lower it by one.

</details>

The question goes inside `<summary>…</summary>`; the answer, with a one or two sentence \
explanation, goes after it, separated by blank lines so it renders as Markdown. Math \
works in both. Nothing comes after the last question.",
        );
    } else {
        s.push_str("\n\nDo not add quiz questions or a \"Check yourself\" section.");
    }
    s
}

fn lesson_minutes(ctx: &LessonContext) -> u32 {
    ctx.outline
        .get(ctx.index)
        .map(|l| l.minutes)
        .filter(|m| *m > 0)
        .unwrap_or(10)
}

pub fn lesson_input(ctx: &LessonContext) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "Subject: {}", ctx.subject_title);
    let _ = writeln!(s, "Topic: {}", ctx.topic_title);
    let _ = writeln!(s, "The learner's request: {}", ctx.prompt.trim());
    let _ = writeln!(s, "\nThe course outline:");
    for (
        i,
        OutlineLesson {
            title,
            summary,
            minutes,
        },
    ) in ctx.outline.iter().enumerate()
    {
        let marker = if i == ctx.index {
            "  <- write this lesson"
        } else {
            ""
        };
        let _ = writeln!(s, "{}. {title} ({minutes} min): {summary}{marker}", i + 1);
    }
    let lesson = &ctx.outline[ctx.index];
    let _ = writeln!(
        s,
        "\nWrite lesson {} of {}: \"{}\".\nIt covers: {}",
        ctx.index + 1,
        ctx.outline.len(),
        lesson.title,
        lesson.summary
    );
    if ctx.index > 0 {
        let _ = writeln!(
            s,
            "The previous lesson is \"{}\"; the learner has read it, so build on it instead \
             of repeating it.",
            ctx.outline[ctx.index - 1].title
        );
    } else {
        let _ = writeln!(
            s,
            "This is the first lesson: assume nothing from the course yet."
        );
    }
    if let Some(next) = ctx.outline.get(ctx.index + 1) {
        let _ = writeln!(
            s,
            "The next lesson is \"{}\"; leave its material for it (a one-line pointer \
             forward is fine).",
            next.title
        );
    } else {
        let _ = writeln!(s, "This is the last lesson of the course.");
    }
    s
}

// ---------------------------------------------------------------------------
// ManimGX scene
// ---------------------------------------------------------------------------

/// A condensed, verified ManimGX (0.1.1) reference: https://manimgx.academa.ai/llms.txt
/// and the user-guide pages on sound and voice, text and math, graphs and the Manim CE
/// differences. Every name below exists in the installed `manimgx` package, and the
/// rules match the fixtures in `tests/fixtures`, which render with the real CLI.
pub const MANIMGX_CHEAT_SHEET: &str = r#"ManimGX reference (it has the API of Manim Community Edition; everything is on the package, `m.<Name>`):

Scene and time
- `class LessonScene(m.Scene):` with `def construct(self) -> None:`.
- Time passes only in `self.say(...)`, `self.play(...)` (1 s unless `run_time=`) and `self.wait(seconds)`. `self.add(mob)` shows a mobject at once without animating it; `self.remove(mob)` hides it.
- The frame is 14.2 x 8 units centred on the origin. Points are `[x, y, 0]`. Keep everything inside x in [-7, 7] and y in [-4, 4], with a margin: aim for x in [-6.5, 6.5] and y in [-3.6, 3.6].
- The background is black by default; light colors (m.WHITE, m.BLUE, m.YELLOW, m.GREEN, m.ORANGE, m.TEAL, m.PINK, m.RED, m.PURPLE, m.GOLD, m.GREY_B) read well on it.

Narration: `self.say`
- `self.say("The derivative of [x squared] is [two x].", m.Indicate(square), m.Write(derivative))` speaks the line and plays each animation during its bracketed words: the first animation during the first [..] span, the second during the second, and so on. The brackets are not spoken. Each animation starts as its words start and lasts as long as they take to say (or its own run_time if longer). The scene waits until the line ends, so lines never overlap.
- The number of [..] spans must equal the number of animations exactly, or the scene fails with "N bracketed spans for M animations". A line with no brackets and no animations is plain narration over a still picture: `self.say("Notice that nothing here depends on n.")`.
- Brackets are reserved for these spans: never nest them and never use [ or ] for anything else in the text (write "the interval from zero to one", not "[0, 1]").
- The text is read aloud by a speech synthesiser and becomes the captions: write it as spoken English, numbers and symbols in words ("x squared", "two pi", "f of x").
- To play several animations during one span, group them: `m.AnimationGroup(m.Create(graph), m.Write(label))`.

Text and math (typeset by ManimGX itself; no LaTeX installation)
- `m.Text("plain words", font_size=48)` (48 is the default; titles 48-56, body 32-40). `t2c={"word": m.YELLOW}` colors a word.
- `m.MathTex(r"\int_0^1 x^2 \, dx = \frac{1}{3}", font_size=48)`: LaTeX math, always in a raw string. Fractions, roots, sums, integrals, limits, matrices, `cases`, Greek letters, `\mathbb`, `\text{...}` work. No LaTeX packages, `\usepackage`, `\def` or `\newcommand`.
- `m.MathTex("a^2", "+", "b^2", "=", "c^2")` makes parts: `formula[2]` is "b^2", to color (`formula[2].set_color(m.YELLOW)`) or animate (`m.Indicate(formula[2])`).
- `m.Tex(r"The area is $\pi r^2$.")` mixes words and math.
- `m.TransformMatchingTex(before, after)` morphs one MathTex into another, moving parts with the same LaTeX.
- `m.DecimalNumber(3.14, num_decimal_places=2)` shows a number.

Shapes
- `m.Dot(point, color=m.YELLOW)`, `m.Line(a, b)`, `m.DashedLine(a, b)`, `m.Arrow(a, b, buff=0)`, `m.DoubleArrow(a, b)`, `m.Circle(radius=1)`, `m.Square(side_length=2)`, `m.Rectangle(width=4, height=2)`, `m.RoundedRectangle(corner_radius=0.2, width=4, height=2)`, `m.Polygon([x1, y1, 0], [x2, y2, 0], [x3, y3, 0])`, `m.Arc(radius=1, start_angle=0, angle=m.PI / 2)`.
- `m.Brace(mob, m.DOWN)` with `brace.get_tex(r"n")` for its label, `m.SurroundingRectangle(mob, color=m.YELLOW, buff=0.15)` to box something.
- Style keywords: `color=`, `fill_opacity=0.5`, `stroke_width=4`. Constants: `m.PI`, `m.TAU`, `m.DEGREES`.

Placement
- `.move_to([x, y, 0])`, `.shift(2 * m.LEFT + m.UP)`, `.to_edge(m.UP)`, `.to_corner(m.UL)`, `.next_to(other, m.RIGHT, buff=0.3)`, `.scale(0.8)`, `.rotate(m.PI / 4)`.
- Directions: `m.UP`, `m.DOWN`, `m.LEFT`, `m.RIGHT`, `m.UL`, `m.UR`, `m.DL`, `m.DR`, `m.ORIGIN`.
- `m.VGroup(a, b, c).arrange(m.DOWN, buff=0.4)` stacks mobjects; a VGroup moves, scales and fades as one.

Graphs
- `axes = m.Axes(x_range=[-3, 3, 1], y_range=[0, 9, 3], x_length=7, y_length=4.5)`; `axes.add_coordinates()` numbers the ticks; `axes.get_axis_labels("x", "y")` labels the axes.
- `graph = axes.plot(lambda x: x**2, x_range=[-3, 3], color=m.BLUE)`. Use plain Python arithmetic; for functions like sine use `import math` and `math.sin`.
- `axes.c2p(2, 4)` is the frame point of the axes' coordinates (2, 4): use it to place dots, lines and labels on a graph.
- `axes.get_graph_label(graph, r"y = x^2", x_val=2, direction=m.UP)` labels a graph (choose `x_val` and `direction` so the label sits clear of the curve), `axes.get_area(graph, x_range=[1, 2], opacity=0.4)`.
- `m.NumberLine(x_range=[-2, 2, 1], length=8, include_numbers=True)`, `m.NumberPlane()` (a grid filling the frame).

Animations
- Appear: `m.Create(mob)` (draws lines and shapes), `m.Write(text_or_math)`, `m.FadeIn(mob, shift=m.UP)`, `m.GrowFromCenter(mob)`, `m.GrowArrow(arrow)`.
- Disappear: `m.FadeOut(mob)`, `m.FadeOut(a, b, c)`, and to clear everything `m.FadeOut(*self.mobjects)`; `m.Uncreate(mob)`.
- Change: `m.Transform(a, b)` (a takes b's shape; keep using a), `m.ReplacementTransform(a, b)` (b replaces a; use b afterwards), `m.TransformFromCopy(a, b)`, `mob.animate.shift(m.RIGHT).set_color(m.RED)` animates any method call, `m.Rotate(mob, m.PI / 2)`, `m.MoveAlongPath(dot, path)`.
- Emphasis: `m.Indicate(mob)`, `m.Circumscribe(mob)`, `m.Flash(point_or_mob)`, `m.Wiggle(mob)`.
- Combine: `m.AnimationGroup(a, b)` (together), `m.LaggedStart(a, b, c, lag_ratio=0.3)`, `m.Succession(a, b)` (one after another).
- Silent steps without narration: `self.play(m.FadeOut(old), run_time=0.8)`, `self.wait(1)`."#;

pub fn scene_instructions() -> String {
    format!(
        r#"You write a narrated explainer video for one lesson of a study app, as a Python scene file for ManimGX, an animation engine with the API of Manim Community Edition.

Answer with the Python source only: no Markdown code fences, no explanation before or after it.

Hard rules (the renderer rejects or breaks on anything else):
1. Imports: `import manimgx as m`, and the standard library's `import math` only if you need it. No other imports. Never `import manim`, never `from manimgx import *`.
2. Exactly one scene class, `class LessonScene(m.Scene):`, with `def construct(self) -> None:`. Helper functions at module level are fine; no other classes.
3. All narration goes through `self.say("...", animations...)`, with one bracketed span per animation, in the same order. Do not use `add_sound`, `m.Sound`, `self.speech` or `add_subcaption`.
4. Do not set a `voice` attribute and do not pass `voice=` to `say`: the renderer installs the voice.
5. No network, no files: do not open, read or write files, no `ImageMobject`, `SVGMobject` or `Code(code_file=...)`, no `open`, `exec`, `eval` or `__import__`, no changes to `m.config`.
6. Everything visible stays inside the frame: x in [-7, 7], y in [-4, 4]. Text never overlaps other text or lies across lines and shapes: give each element its own place, and remove or move old elements before reusing their space.
7. Total length between 60 and 150 seconds. Narration is about 2.5 spoken words per second, so write 150 to 350 words of narration in roughly 8 to 18 `say` lines of one or two sentences each.
8. End with the poster shot, which becomes the video's thumbnail: after the last `say` line, clear the board, then bring the lesson title (at the top, `font_size=56`, `.to_edge(m.UP)`) and the lesson's single most important diagram or formula (large, below the title; a diagram may have its one key formula beside it) onto the screen together in one `self.play(...)`, and make `self.wait(2)` the last statement of `construct`. Nothing fades out or changes after it: no `FadeOut` at the end. Keep the poster shot uncluttered (the title and that one picture or formula, nothing else, font sizes of at least 48) and inside the frame, so it reads at thumbnail size.

How to make it good:
- Teach the lesson's central idea visually, not the whole lesson: one clear story with a beginning (the question), a middle (the picture or derivation that answers it) and an end (the takeaway, restated).
- Show, then say: every `say` line should change or highlight something on screen, and every bracketed span should be the words that describe what its animation shows.
- Build the picture up step by step; keep at most about 6 elements on screen at once. Clear the board (`m.FadeOut(*self.mobjects)`) between parts of the story.
- Put a short title at the top (`.to_edge(m.UP)`), the main picture on the left or centre, formulas on the right or below, and keep font sizes readable (at least 30).
- Use the lesson's own notation and a concrete example with real numbers where it helps.
- Prefer simple, reliable constructions from the reference below. Do not invent methods or keyword arguments; if unsure, use a simpler construction.

{cheat_sheet}

A complete, valid file showing the expected shape (much shorter than a real scene):

{example}"#,
        cheat_sheet = MANIMGX_CHEAT_SHEET,
        example = EXAMPLE_SCENE,
    )
}

/// The example scene shown to the model. It is a test fixture so that it is rendered
/// with the real ManimGX CLI (see `tests/fixtures/README.md`).
pub const EXAMPLE_SCENE: &str = include_str!("../tests/fixtures/prompt_example_scene.py");

pub fn scene_input(req: &SceneRequest) -> String {
    let mut s = format!(
        "Lesson title: {}\n\nLesson (Markdown, math in $...$):\n\n{}\n",
        req.lesson_title.trim(),
        req.lesson_body_md.trim()
    );
    if let Some(prev) = &req.previous_attempt {
        let _ = write!(
            s,
            "\n---\n\nA previous version of the scene failed. Repair it: keep its story, \
narration and layout where they work, fix the cause of the error below and anything else \
that breaks the hard rules, and answer with the complete corrected file.\n\n\
The error ManimGX reported:\n{}\n\nThe previous scene:\n{}\n",
            prev.error.trim(),
            prev.code.trim()
        );
    } else {
        s.push_str("\nWrite the scene for this lesson.\n");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(index: usize, with_quiz: bool) -> LessonContext {
        LessonContext {
            subject_title: "Calculus".into(),
            topic_title: "Derivatives".into(),
            prompt: "teach me derivatives".into(),
            level: Level::Intro,
            with_quiz,
            outline: vec![
                OutlineLesson {
                    title: "Slopes".into(),
                    summary: "s1".into(),
                    minutes: 8,
                },
                OutlineLesson {
                    title: "Limits".into(),
                    summary: "s2".into(),
                    minutes: 12,
                },
                OutlineLesson {
                    title: "Rules".into(),
                    summary: "s3".into(),
                    minutes: 10,
                },
            ],
            index,
        }
    }

    #[test]
    fn lesson_prompt_mentions_neighbours_and_quiz() {
        let c = ctx(1, true);
        let input = lesson_input(&c);
        assert!(input.contains("\"Slopes\""));
        assert!(input.contains("\"Rules\""));
        assert!(input.contains("Write lesson 2 of 3: \"Limits\""));
        let ins = lesson_instructions(&c);
        assert!(ins.contains("## Check yourself"));
        assert!(ins.contains("<details><summary>"));
        assert!(ins.contains("about 12 minutes"));
        assert!(!lesson_instructions(&ctx(0, false)).contains("exactly `## Check yourself`"));
    }

    #[test]
    fn outline_prompt_states_the_count() {
        assert!(outline_instructions(5, Level::Advanced).contains("exactly 5 lessons"));
        assert!(outline_instructions(1, Level::Intro).contains("exactly 1 lesson,"));
    }

    #[test]
    fn scene_prompt_asks_for_a_held_poster_shot() {
        let ins = scene_instructions();
        assert!(ins.contains("8. End with the poster shot"));
        assert!(ins.contains("`self.wait(2)` the last statement of `construct`"));
        // The example the model copies ends that way too.
        assert!(EXAMPLE_SCENE.trim_end().ends_with("self.wait(2)"));
        assert!(ins.contains(EXAMPLE_SCENE));
    }

    #[test]
    fn repair_input_carries_code_and_error() {
        let req = SceneRequest {
            lesson_title: "T".into(),
            lesson_body_md: "Body".into(),
            previous_attempt: Some(crate::FailedScene {
                code: "import manimgx as m".into(),
                error: "NameError: ShowCreation".into(),
            }),
        };
        let input = scene_input(&req);
        assert!(input.contains("Repair it"));
        assert!(input.contains("NameError: ShowCreation"));
        assert!(input.contains("import manimgx as m"));
    }
}
