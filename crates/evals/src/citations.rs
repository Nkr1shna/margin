//! Deterministic checks on generated lessons and answers (§3.2 "Citation accuracy",
//! "Structure checks"): every `[^n]` resolves to a passage that was sent, the cited
//! passage shares at least two content terms with the sentence citing it (a lexical
//! support proxy), the must-cite documents are cited, and the lesson keeps the rules
//! its prompt states. Used by the CI gate over recorded generations and by the runner.

use std::collections::{BTreeSet, HashSet};

use serde::{Deserialize, Serialize};

/// Words that carry no content.
const STOP: &[&str] = &[
    "about", "above", "after", "again", "against", "also", "because", "been", "before", "being",
    "below", "between", "both", "but", "came", "can", "come", "could", "does", "doing", "down",
    "during", "each", "even", "every", "from", "further", "have", "having", "here", "how", "into",
    "itself", "just", "like", "made", "make", "many", "more", "most", "much", "must", "only",
    "other", "over", "same", "should", "since", "some", "such", "than", "that", "their", "them",
    "then", "there", "these", "they", "this", "those", "through", "under", "until", "very",
    "what", "when", "where", "which", "while", "will", "with", "would", "your", "yours", "the",
    "and", "for", "are", "was", "were", "its", "not", "one", "two", "use", "used", "uses",
    "using", "way", "ways", "thing", "things", "lesson", "example", "source", "sources",
];

/// The content terms of `text`: lower-case words of four letters or more (or with a
/// digit), not stop words, with a plural `s` dropped.
pub fn content_terms(text: &str) -> HashSet<String> {
    text.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .filter(|w| !w.is_empty())
        .map(str::to_lowercase)
        .filter(|w| w.chars().count() >= 4 || w.chars().any(|c| c.is_ascii_digit()))
        .filter(|w| !STOP.contains(&w.as_str()))
        .map(|w| {
            if w.len() > 4 && w.ends_with('s') && !w.ends_with("ss") {
                w[..w.len() - 1].to_string()
            } else {
                w
            }
        })
        .collect()
}

/// A sentence of prose and the passages it cites.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sentence {
    pub text: String,
    /// `n` of each `[^n]` (1-based, as written).
    pub markers: Vec<usize>,
}

/// The prose sentences of a Markdown body, outside code fences, headings and footnote
/// definitions, with the `[^n]` markers each carries (markers right after a sentence's
/// end belong to it).
pub fn sentences(body: &str) -> Vec<Sentence> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let mut prose = String::new();
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            prose.push('\n');
            continue;
        }
        if in_fence || t.starts_with('#') || t.starts_with("[^") || t == "$$" {
            prose.push('\n');
            continue;
        }
        prose.push_str(line);
        prose.push('\n');
    }
    let chars: Vec<char> = prose.chars().collect();
    let mut cur = String::new();
    let mut markers = Vec::new();
    let mut i = 0;
    let flush = |cur: &mut String, markers: &mut Vec<usize>, out: &mut Vec<Sentence>| {
        let text = cur.split_whitespace().collect::<Vec<_>>().join(" ");
        if text.chars().any(char::is_alphanumeric) {
            out.push(Sentence {
                text,
                markers: std::mem::take(markers),
            });
        } else {
            markers.clear();
        }
        cur.clear();
    };
    while i < chars.len() {
        // A marker.
        if chars[i] == '[' && chars.get(i + 1) == Some(&'^') {
            let mut j = i + 2;
            let mut n = String::new();
            while j < chars.len() && chars[j].is_ascii_digit() {
                n.push(chars[j]);
                j += 1;
            }
            if !n.is_empty() && chars.get(j) == Some(&']') {
                markers.push(n.parse().unwrap_or(0));
                i = j + 1;
                continue;
            }
        }
        let c = chars[i];
        cur.push(c);
        let blank_line = c == '\n' && chars.get(i + 1) == Some(&'\n');
        if matches!(c, '.' | '!' | '?') || blank_line {
            // Take markers that follow the end directly.
            let mut j = i + 1;
            loop {
                if chars.get(j) == Some(&'[') && chars.get(j + 1) == Some(&'^') {
                    let mut k = j + 2;
                    let mut n = String::new();
                    while k < chars.len() && chars[k].is_ascii_digit() {
                        n.push(chars[k]);
                        k += 1;
                    }
                    if !n.is_empty() && chars.get(k) == Some(&']') {
                        markers.push(n.parse().unwrap_or(0));
                        j = k + 1;
                        continue;
                    }
                }
                break;
            }
            let ends = blank_line || chars.get(j).is_none_or(|c| c.is_whitespace());
            if ends {
                i = j;
                flush(&mut cur, &mut markers, &mut out);
                continue;
            }
        }
        i += 1;
    }
    flush(&mut cur, &mut markers, &mut out);
    out
}

/// What was sent with a generation: per passage (in the order numbered `[^1]`, `[^2]` …)
/// its document and text.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentPassage {
    pub document: String,
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CitationReport {
    pub markers: usize,
    /// Markers naming a passage that was sent.
    pub resolved: usize,
    /// Resolved markers whose passage shares ≥ 2 content terms with the sentence.
    pub supported: usize,
    pub sentences: usize,
    pub cited_sentences: usize,
    /// Must-cite documents, and those cited.
    pub must_cite: usize,
    pub must_cite_hit: usize,
    /// Markers that don't resolve, as written.
    pub unresolved: Vec<usize>,
}

impl CitationReport {
    /// Points (0–100); 100 when there is nothing to resolve.
    pub fn resolution_pct(&self) -> f64 {
        pct(self.resolved, self.markers)
    }
    pub fn lexical_support_pct(&self) -> f64 {
        pct(self.supported, self.resolved)
    }
    pub fn cited_sentence_pct(&self) -> f64 {
        pct(self.cited_sentences, self.sentences)
    }
    pub fn must_cite_pct(&self) -> f64 {
        pct(self.must_cite_hit, self.must_cite)
    }

    pub fn add(&mut self, o: &CitationReport) {
        self.markers += o.markers;
        self.resolved += o.resolved;
        self.supported += o.supported;
        self.sentences += o.sentences;
        self.cited_sentences += o.cited_sentences;
        self.must_cite += o.must_cite;
        self.must_cite_hit += o.must_cite_hit;
        self.unresolved.extend(&o.unresolved);
    }
}

fn pct(num: usize, den: usize) -> f64 {
    if den == 0 {
        100.0
    } else {
        num as f64 * 100.0 / den as f64
    }
}

/// Checks the citations of `body` against the passages sent with it.
pub fn check(body: &str, sent: &[SentPassage], must_cite: &[String]) -> CitationReport {
    let mut r = CitationReport::default();
    let mut cited_docs: BTreeSet<&str> = BTreeSet::new();
    let passage_terms: Vec<HashSet<String>> = sent.iter().map(|p| content_terms(&p.text)).collect();
    for s in sentences(body) {
        r.sentences += 1;
        if !s.markers.is_empty() {
            r.cited_sentences += 1;
        }
        let terms = content_terms(&s.text);
        for &n in &s.markers {
            r.markers += 1;
            match n.checked_sub(1).and_then(|i| sent.get(i).map(|p| (i, p))) {
                Some((i, p)) => {
                    r.resolved += 1;
                    cited_docs.insert(p.document.as_str());
                    if terms.intersection(&passage_terms[i]).count() >= 2 {
                        r.supported += 1;
                    }
                }
                None => r.unresolved.push(n),
            }
        }
    }
    r.must_cite = must_cite.len();
    r.must_cite_hit = must_cite
        .iter()
        .filter(|d| cited_docs.contains(d.as_str()))
        .count();
    r
}

/// The lesson rules the writing prompt states, as a cheap proxy for prompt regressions:
/// no `#` title, exactly one key-idea callout, a worked example under its own heading,
/// and the "Check yourself" questions exactly when a quiz was asked for.
pub fn check_structure(body: &str, with_quiz: bool) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_fence = false;
    let (mut key_ideas, mut example, mut check_heading) = (0, false, false);
    for line in body.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if t.starts_with("# ") {
            out.push("starts a line with a `# ` title".to_string());
        }
        if t.starts_with('>') && t.contains("**Key idea.**") {
            key_ideas += 1;
        }
        if let Some(h) = t.strip_prefix("## ") {
            let h = h.to_lowercase();
            if h.contains("example") {
                example = true;
            }
            if h.trim() == "check yourself" {
                check_heading = true;
            }
        }
    }
    if key_ideas != 1 {
        out.push(format!("{key_ideas} key-idea callouts, not exactly one"));
    }
    if !example {
        out.push("no worked example under its own `## ` heading".to_string());
    }
    let questions = body.matches("<details>").count();
    if with_quiz {
        if !check_heading {
            out.push("no `## Check yourself` section".to_string());
        }
        if !(2..=4).contains(&questions) {
            out.push(format!("{questions} quiz questions, not 2 to 4"));
        }
    } else if check_heading || questions > 0 {
        out.push("quiz questions although none were asked for".to_string());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sent() -> Vec<SentPassage> {
        vec![
            SentPassage {
                document: "fourier-series.md".into(),
                text: "Near a jump discontinuity the partial sums overshoot by about nine \
                       percent. Adding more terms does not shrink the overshoot."
                    .into(),
            },
            SentPassage {
                document: "heat-engines.rst".into(),
                text: "The Carnot efficiency is one minus the ratio of the cold and hot \
                       reservoir temperatures."
                    .into(),
            },
        ]
    }

    #[test]
    fn sentences_carry_their_markers() {
        let body = "Intro line without a marker.\n\n## Heading [^9]\n\nThe partial sums \
                    overshoot near a jump.[^1] Adding terms never removes it [^1][^2].\n\n\
                    ```\nx = 1. [^5]\n```\n\n[^1]: a definition the writer added.\n- a list item[^2]\n";
        let s = sentences(body);
        let texts: Vec<&str> = s.iter().map(|s| s.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "Intro line without a marker.",
                "The partial sums overshoot near a jump.",
                "Adding terms never removes it .",
                "- a list item",
            ]
        );
        assert_eq!(s[1].markers, vec![1]);
        assert_eq!(s[2].markers, vec![1, 2]);
        assert_eq!(s[3].markers, vec![2]);
        // Decimals don't end a sentence.
        assert_eq!(sentences("It is 0.09 of the jump.[^1]").len(), 1);
    }

    #[test]
    fn citation_checks_by_hand() {
        let body = "Near a jump the partial sums overshoot by nine percent.[^1] \
                    Efficiency depends on reservoir temperatures.[^2] \
                    Fourier invented the guillotine.[^2] And a marker to nowhere.[^7]";
        let r = check(body, &sent(), &["fourier-series.md".into(), "bayes.md".into()]);
        assert_eq!(r.markers, 4);
        assert_eq!(r.resolved, 3);
        assert_eq!(r.unresolved, vec![7]);
        // [^1]: jump, partial, sum, overshoot, nine, percent; [^2] first: efficiency,
        // reservoir, temperature; [^2] second: nothing shared.
        assert_eq!(r.supported, 2);
        assert_eq!(r.sentences, 4);
        assert_eq!(r.cited_sentences, 4);
        assert_eq!((r.must_cite, r.must_cite_hit), (2, 1));
        assert_eq!(r.resolution_pct(), 75.0);
        assert!((r.lexical_support_pct() - 66.666).abs() < 0.01);
        assert_eq!(r.must_cite_pct(), 50.0);
        let mut total = CitationReport::default();
        total.add(&r);
        total.add(&r);
        assert_eq!(total.markers, 8);
        assert_eq!(CitationReport::default().resolution_pct(), 100.0);
    }

    #[test]
    fn content_terms_skip_stop_words_and_plurals() {
        let t = content_terms("The reservoirs and their temperatures, with 3 cases of RC");
        assert!(t.contains("reservoir"));
        assert!(t.contains("temperature"));
        assert!(t.contains("3"));
        assert!(t.contains("case"));
        assert!(!t.contains("their"));
        assert!(!t.contains("the"));
        assert!(!t.contains("rc"), "short words without digits are skipped");
    }

    #[test]
    fn structure_checks() {
        let good = "Why it matters.\n\n## The idea\n\nText.\n\n> **Key idea.** One line.\n\n\
                    ## Worked example\n\nSteps.\n\n## Check yourself\n\n<details><summary>Q1</summary>\n\nA.\n\n</details>\n\n\
                    <details><summary>Q2</summary>\n\nA.\n\n</details>\n";
        assert_eq!(check_structure(good, true), Vec::<String>::new());
        let v = check_structure(good, false).join("\n");
        assert!(v.contains("none were asked for"));
        let bad = "# Title\n\n> **Key idea.** a\n\n> **Key idea.** b\n\n```\n# not a title\n```\n";
        let v = check_structure(bad, true).join("\n");
        for want in [
            "`# ` title",
            "2 key-idea callouts",
            "no worked example",
            "no `## Check yourself`",
            "0 quiz questions",
        ] {
            assert!(v.contains(want), "{want} not in {v}");
        }
        assert_eq!(check_structure(bad, true).iter().filter(|l| l.contains("title")).count(), 1);
    }
}
