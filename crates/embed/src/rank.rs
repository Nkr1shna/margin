//! The exact-term part of `score`: +0.05 for each distinct query term found in the
//! chunk, at most +0.15. A term is a query word of three or more characters that isn't
//! a stop word, or any word with a digit (invoice numbers, years, versions).

use std::collections::HashSet;

/// Added for each distinct query term found in the chunk.
const TERM_BOOST: f32 = 0.05;
const MAX_TERM_BOOST: f32 = 0.15;

/// Common words that say nothing about a passage.
const STOP_WORDS: &[&str] = &[
    "the", "and", "for", "are", "but", "not", "you", "all", "any", "can", "had", "her", "was",
    "one", "our", "out", "has", "his", "how", "its", "who", "did", "does", "what", "when", "where",
    "which", "why", "with", "this", "that", "from", "they", "them", "then", "than", "there",
    "these", "those", "into", "about", "have", "were", "will", "would", "should", "could", "your",
    "their", "been", "also", "some", "such", "more", "most", "other", "only", "over", "very",
    "just", "each", "both", "between", "after", "before", "because", "while",
];

pub(crate) fn term_boost(query: &str, chunk: &str) -> f32 {
    let words = words(chunk);
    let matched = terms(query)
        .iter()
        .filter(|t| words.contains(t.as_str()))
        .count();
    (TERM_BOOST * matched as f32).min(MAX_TERM_BOOST)
}

/// A word lowercased and trimmed of surrounding punctuation: "(INV-2024-001)," →
/// "inv-2024-001".
fn clean(raw: &str) -> String {
    raw.trim_matches(|c: char| !c.is_alphanumeric())
        .to_lowercase()
}

/// Words of `text`, kept whole ("inv-2024-001") and also split at inner punctuation
/// ("inv", "2024", "001").
fn words(text: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    for word in text.split_whitespace().map(clean).filter(|w| !w.is_empty()) {
        for part in word.split(|c: char| !c.is_alphanumeric()) {
            if !part.is_empty() && part.len() != word.len() {
                out.insert(part.to_string());
            }
        }
        out.insert(word);
    }
    out
}

/// The query's distinct terms.
fn terms(query: &str) -> Vec<String> {
    let mut terms: Vec<String> = query
        .split_whitespace()
        .map(clean)
        .filter(|w| {
            w.chars().any(|c| c.is_numeric())
                || (w.chars().count() >= 3 && !STOP_WORDS.contains(&w.as_str()))
        })
        .collect();
    terms.sort();
    terms.dedup();
    terms
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::score;

    /// A unit vector at cosine `cos` to `[1, 0]`.
    fn at(cos: f32) -> Vec<f32> {
        vec![cos, (1.0 - cos * cos).sqrt()]
    }

    const Q: [f32; 2] = [1.0, 0.0];

    #[test]
    fn score_is_the_cosine_when_no_term_matches() {
        assert!((score(&Q, "is it of", &at(0.6), "is it of") - 0.6).abs() < 1e-6);
        assert!((score(&Q, "volcano", &at(0.3), "a passage about rivers") - 0.3).abs() < 1e-6);
    }

    #[test]
    fn each_distinct_term_adds_a_little_up_to_a_cap() {
        let chunk = "Invoice INV-2024-001 from Acme Corp, due 2024-03-01. Total: 42 EUR.";
        let boost = |query: &str| score(&Q, query, &at(0.5), chunk) - 0.5;
        let close = |a: f32, b: f32| (a - b).abs() < 1e-6;
        assert!(close(boost("acme"), 0.05));
        // Case and punctuation don't matter; a repeated term counts once.
        assert!(close(boost("ACME? acme, Acme"), 0.05));
        // Identifiers match whole or by their parts, and short numbers count.
        assert!(close(boost("inv-2024-001"), 0.05));
        assert!(close(boost("2024"), 0.05));
        assert!(close(boost("42"), 0.05));
        assert!(close(boost("acme invoice"), 0.10));
        assert!(close(boost("acme invoice total 42 eur"), 0.15));
        // Short words, stop words and words not in the chunk don't count.
        assert!(close(boost("of to it from"), 0.0));
        assert!(close(boost("volcano 99"), 0.0));
    }

    #[test]
    fn an_exact_match_can_outrank_a_slightly_closer_vector() {
        let near = score(&Q, "invoice 48213", &at(0.62), "An invoice from last year.");
        let exact = score(&Q, "invoice 48213", &at(0.58), "Invoice 48213: two chairs.");
        assert!(exact > near, "{exact} vs {near}");
    }

    #[test]
    fn words_and_terms() {
        let w = words("See (INV-2024-001), snake_case.");
        for expected in [
            "see",
            "inv-2024-001",
            "inv",
            "2024",
            "001",
            "snake_case",
            "snake",
            "case",
        ] {
            assert!(w.contains(expected), "{expected} in {w:?}");
        }
        assert_eq!(
            terms("What is the INV-2024-001 total? total"),
            vec!["inv-2024-001", "total"]
        );
    }
}
