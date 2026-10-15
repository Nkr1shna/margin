//! Cleanup of model output that ignores a formatting instruction: Markdown code fences
//! around an answer that should be bare, and a `# Title` the app already shows.

/// The contents of a fence that wraps the whole of `text` (```` ```lang … ``` ````),
/// when `accept_lang` accepts its language tag. `None` when `text` isn't wrapped.
fn wrapped(text: &str, accept_lang: impl Fn(&str) -> bool) -> Option<&str> {
    let t = text.trim();
    let rest = t.strip_prefix("```")?;
    let (lang, body) = rest.split_once('\n')?;
    if !accept_lang(lang.trim()) {
        return None;
    }
    let body = body.trim_end();
    let body = body.strip_suffix("```")?;
    // A wrapped answer has no fence of its own at column 0 inside; if it has, the
    // first and last fences belong to different blocks.
    if body.lines().any(|l| l.trim_start().starts_with("```")) {
        return None;
    }
    Some(body.trim_matches('\n'))
}

/// The fenced blocks in `text`, in order: (language tag, contents).
fn fenced_blocks(text: &str) -> Vec<(&str, String)> {
    let mut out = Vec::new();
    let mut lines = text.lines();
    while let Some(line) = lines.next() {
        let Some(lang) = line.trim_start().strip_prefix("```") else {
            continue;
        };
        let mut body = Vec::new();
        for inner in lines.by_ref() {
            if inner.trim_start().starts_with("```") {
                break;
            }
            body.push(inner);
        }
        out.push((lang.trim(), body.join("\n")));
    }
    out
}

/// Python source from a model answer: the bare code, or the code inside the fence the
/// model put it in by mistake (the block that defines `LessonScene` when there are
/// several, or prose around it).
pub fn scene_code(answer: &str) -> String {
    if let Some(body) = wrapped(answer, |_| true) {
        return format!("{}\n", body.trim_end());
    }
    let t = answer.trim();
    if t.contains("```") {
        let blocks = fenced_blocks(t);
        if let Some((_, code)) = blocks
            .iter()
            .find(|(_, code)| code.contains("class LessonScene"))
        {
            return format!("{}\n", code.trim_matches('\n').trim_end());
        }
    }
    format!("{t}\n")
}

/// A lesson body from a model answer: removes a fence wrapping the whole answer and a
/// leading `# ` title line (the app shows the title itself).
pub fn lesson_markdown(answer: &str) -> String {
    let body = wrapped(answer, |lang| {
        lang.is_empty() || lang.eq_ignore_ascii_case("markdown") || lang.eq_ignore_ascii_case("md")
    })
    .unwrap_or(answer)
    .trim();
    let body = match body.split_once('\n') {
        Some((first, rest)) if first.starts_with("# ") => rest.trim_start(),
        None if body.starts_with("# ") => "",
        _ => body,
    };
    format!("{}\n", body.trim_end())
}

/// The JSON of a structured answer: the text itself, or the inside of a code fence a
/// compatible server wrapped it in.
pub fn json_answer(text: &str) -> &str {
    let t = text.trim();
    let Some(rest) = t.strip_prefix("```") else {
        return t;
    };
    let Some((lang, body)) = rest.split_once('\n') else {
        return t;
    };
    if !matches!(lang.trim(), "" | "json") {
        return t;
    }
    body.trim_end().strip_suffix("```").map_or(t, str::trim)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CODE: &str = "import manimgx as m\n\n\nclass LessonScene(m.Scene):\n    def construct(self) -> None:\n        self.wait(1)\n";

    #[test]
    fn bare_code_is_kept() {
        assert_eq!(scene_code(CODE), CODE);
        assert_eq!(scene_code(&format!("\n\n{CODE}\n\n")), CODE);
    }

    #[test]
    fn python_fence_is_stripped() {
        assert_eq!(scene_code(&format!("```python\n{CODE}```")), CODE);
        assert_eq!(scene_code(&format!("```py\n{CODE}```\n")), CODE);
        assert_eq!(scene_code(&format!("```\n{CODE}\n```")), CODE);
    }

    #[test]
    fn prose_around_a_fence_is_dropped() {
        let answer =
            format!("Here is the scene:\n\n```python\n{CODE}```\n\nIt runs for 90 seconds.");
        assert_eq!(scene_code(&answer), CODE);
    }

    #[test]
    fn the_lesson_scene_block_wins() {
        let answer = format!("```bash\nmanimgx render scene.py\n```\n\n```python\n{CODE}```");
        assert_eq!(scene_code(&answer), CODE);
    }

    #[test]
    fn markdown_fence_and_title_are_stripped() {
        let body = "## Intuition\n\nA paragraph with $x^2$.\n";
        assert_eq!(lesson_markdown(body), body);
        assert_eq!(lesson_markdown(&format!("```markdown\n{body}```")), body);
        assert_eq!(lesson_markdown(&format!("```md\n{body}\n```\n")), body);
        assert_eq!(
            lesson_markdown(&format!("# Fourier series\n\n{body}")),
            body
        );
    }

    #[test]
    fn inner_code_blocks_are_kept() {
        let body = "## Code\n\n```python\nprint(1)\n```\n\nAfter.\n";
        assert_eq!(lesson_markdown(body), body);
        let fenced_at_both_ends = "```python\nprint(1)\n```\n\nText.\n\n```python\nprint(2)\n```\n";
        assert_eq!(lesson_markdown(fenced_at_both_ends), fenced_at_both_ends);
    }
}
