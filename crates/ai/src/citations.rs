//! Footnote citations in a finished lesson. The writer cites passage n as `[^n]`; the
//! app numbers citations in reading order, so markers are renumbered by first
//! appearance (`[^5] … [^2] … [^5]` → `[^1] … [^2] … [^1]`). Markers that don't name a
//! passage, footnote definitions the writer added anyway (`[^1]: …`) and repeats of the
//! same marker side by side are dropped. Code (fenced blocks and inline spans) is left
//! alone.

/// A lesson body with its citations renumbered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Renumbered {
    pub body: String,
    /// `cited[n - 1]` is the 0-based index of the passage `[^n]` now refers to.
    pub cited: Vec<usize>,
}

/// Renumbers the `[^n]` markers of `body` that name one of `passages` passages (1-based)
/// in order of first appearance and drops the rest (see the module docs).
pub fn renumber_citations(body: &str, passages: usize) -> Renumbered {
    let mut cited: Vec<usize> = Vec::new();
    let mut out = String::with_capacity(body.len());
    let mut fence: Option<String> = None;
    let mut in_definition = false;
    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if let Some(open) = &fence {
            out.push_str(line);
            // Closed by a bare run of the same character, at least as long.
            if fence_start(trimmed).is_some_and(|close| {
                close.starts_with(open.as_str()) && trimmed.trim_end().len() == close.len()
            }) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_start(trimmed) {
            fence = Some(open);
            in_definition = false;
            out.push_str(line);
            continue;
        }
        // A definition and its indented continuation lines, until a blank line.
        if in_definition {
            if line.trim().is_empty() {
                in_definition = false;
            } else if line.starts_with("    ") || line.starts_with('\t') {
                continue;
            } else {
                in_definition = false;
            }
        }
        if line.len() - trimmed.len() <= 3 && is_definition(trimmed) {
            in_definition = true;
            continue;
        }
        rewrite_line(line, passages, &mut cited, &mut out);
    }
    // Dropped definitions at the end leave blank lines; keep one final newline if the
    // body had one.
    out.truncate(out.trim_end().len());
    if body.ends_with('\n') && !out.is_empty() {
        out.push('\n');
    }
    Renumbered { body: out, cited }
}

/// The fence a line opens (```` ``` ```` or `~~~`, three or more).
fn fence_start(trimmed: &str) -> Option<String> {
    for c in ['`', '~'] {
        let run = trimmed.chars().take_while(|&x| x == c).count();
        if run >= 3 {
            return Some(std::iter::repeat_n(c, run).collect());
        }
    }
    None
}

/// `[^label]: …` at the start of a line.
fn is_definition(trimmed: &str) -> bool {
    trimmed
        .strip_prefix("[^")
        .and_then(|rest| rest.split_once("]:"))
        .is_some_and(|(label, _)| !label.is_empty() && !label.contains([']', '[', ' ']))
}

/// Rewrites the markers of one line outside code spans.
fn rewrite_line(line: &str, passages: usize, cited: &mut Vec<usize>, out: &mut String) {
    let bytes = line.as_bytes();
    let mut i = 0;
    let mut last_marker: Option<(usize, u32)> = None; // (end in out, number)
    while i < bytes.len() {
        // Inline code: copy through the matching run of backticks.
        if bytes[i] == b'`' {
            let run = bytes[i..].iter().take_while(|&&b| b == b'`').count();
            let ticks = &line[i..i + run];
            let close = line[i + run..].find(ticks).map(|p| i + run + p + run);
            let end = close.unwrap_or(i + run);
            out.push_str(&line[i..end]);
            i = end;
            last_marker = None;
            continue;
        }
        if line[i..].starts_with("[^")
            && let Some(close) = line[i + 2..].find(']')
        {
            let label = &line[i + 2..i + 2 + close];
            let end = i + 2 + close + 1;
            if !label.is_empty() && !label.contains(['[', ' ']) {
                let passage = label
                    .parse::<usize>()
                    .ok()
                    .filter(|n| (1..=passages).contains(n));
                match passage {
                    Some(n) => {
                        let index = n - 1;
                        let number = match cited.iter().position(|&c| c == index) {
                            Some(p) => p as u32 + 1,
                            None => {
                                cited.push(index);
                                cited.len() as u32
                            }
                        };
                        // `[^2][^2]` once.
                        if last_marker != Some((out.len(), number)) {
                            out.push_str(&format!("[^{number}]"));
                            last_marker = Some((out.len(), number));
                        }
                    }
                    None => {
                        // Dropped; don't leave "word ." behind.
                        let next = line[end..].chars().next();
                        if out.ends_with(' ')
                            && next.is_none_or(|c| c.is_whitespace() || ".,;:!?)".contains(c))
                        {
                            out.pop();
                        }
                    }
                }
                i = end;
                continue;
            }
        }
        let c = line[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renumbers_in_order_of_first_appearance() {
        let body = "Intro cites the third.[^3]\n\nThen the first[^1] and the third again.[^3]\n";
        let r = renumber_citations(body, 4);
        assert_eq!(
            r.body,
            "Intro cites the third.[^1]\n\nThen the first[^2] and the third again.[^1]\n"
        );
        assert_eq!(r.cited, vec![2, 0]);
    }

    #[test]
    fn drops_bad_markers_definitions_and_repeats() {
        let body = "One.[^9] Two [^0]. Three[^x] four.[^2][^2] Five.[^1][^2]\n\n\
                    [^1]: The writer's own footnote.\n    continued here\n\n\
                    [^2]: Another.\n";
        let r = renumber_citations(body, 2);
        assert_eq!(r.body, "One. Two. Three four.[^1] Five.[^2][^1]\n");
        assert_eq!(r.cited, vec![1, 0]);
    }

    #[test]
    fn code_is_left_alone() {
        let body = "Use `a[^1]` in code.[^1]\n\n```python\nx = y[^2]\n```\n\nAfter.[^2]";
        let r = renumber_citations(body, 2);
        assert_eq!(
            r.body,
            "Use `a[^1]` in code.[^1]\n\n```python\nx = y[^2]\n```\n\nAfter.[^2]"
        );
        assert_eq!(r.cited, vec![0, 1]);
        let tildes = renumber_citations("~~~\n[^1]: not a footnote\n~~~\n", 1);
        assert_eq!(tildes.body, "~~~\n[^1]: not a footnote\n~~~\n");
        assert!(tildes.cited.is_empty());
    }

    #[test]
    fn bodies_without_markers_are_unchanged() {
        for body in [
            "",
            "Plain text.",
            "Math $a[^2]$? no: [x](y) and [^ spaced]\n",
        ] {
            let r = renumber_citations(body, 0);
            assert!(r.cited.is_empty());
            if !body.contains("[^2]") {
                assert_eq!(r.body, body);
            }
        }
        // Without passages every marker goes.
        assert_eq!(renumber_citations("A.[^1]", 0).body, "A.");
        // Unicode survives.
        assert_eq!(
            renumber_citations("Café — naïve.[^1] 東京", 1).body,
            "Café — naïve.[^1] 東京"
        );
    }
}
