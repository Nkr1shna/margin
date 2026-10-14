//! Packing gates (§3.3): what a packer hands a prompt never exceeds its token budget or
//! picture count, never carries one unit twice (a page's text and its picture), keeps
//! the per-document cap, and every passage has a locator; and the hits the context API
//! and the agent's `search` tool return for one query fit the tool output limit.

use std::collections::{HashMap, HashSet};

use margin_service::pipeline::{Hit, PackBudget, Packed};
use margin_shared::context::{self, Modality};
use margin_shared::routes::{TOOL_OUTPUT_PASSAGE_CHARS, TOOL_SEARCH_K_MAX};

fn is_picture(p: &Packed) -> bool {
    p.hits.iter().any(|h| h.passage.modality == "picture") || p.citation.image_url.is_some()
}

/// Every way `packed` breaks `budget`; empty = pass.
pub fn check_packing(packed: &[Packed], budget: &PackBudget) -> Vec<String> {
    let mut out = Vec::new();
    let tokens: usize = packed.iter().map(|p| p.tokens).sum();
    if tokens > budget.tokens {
        out.push(format!(
            "{tokens} tokens packed into a budget of {}",
            budget.tokens
        ));
    }
    let pictures = packed.iter().filter(|p| is_picture(p)).count();
    if pictures > budget.images_max {
        out.push(format!(
            "{pictures} pictures packed, at most {}",
            budget.images_max
        ));
    }
    if packed.len() > budget.k {
        out.push(format!(
            "{} passages packed, at most k = {}",
            packed.len(),
            budget.k
        ));
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for (i, p) in packed.iter().enumerate() {
        let units: HashSet<&str> = p.hits.iter().map(|h| h.passage.unit_id.as_str()).collect();
        for u in units {
            if let Some(j) = seen.insert(u, i) {
                out.push(format!("unit {u} is in passages {} and {}", j + 1, i + 1));
            }
        }
        if p.citation.document_id.as_deref().is_none_or(str::is_empty) {
            out.push(format!("passage {} has no document locator", i + 1));
        }
    }
    let cap = budget
        .diversity
        .per_document_max
        .unwrap_or(budget.k.div_ceil(2))
        .max(1);
    let mut per_doc: HashMap<&str, usize> = HashMap::new();
    for p in packed {
        if let Some(d) = p.citation.document_id.as_deref() {
            *per_doc.entry(d).or_default() += 1;
        }
    }
    let mut over: Vec<_> = per_doc.into_iter().filter(|(_, n)| *n > cap).collect();
    over.sort();
    for (d, n) in over {
        out.push(format!("{n} passages from {d}, at most {cap} per document"));
    }
    out
}

/// A ranked hit as the context API (and the agent's `search` tool) returns it, its
/// text cut to `TOOL_OUTPUT_PASSAGE_CHARS`.
pub fn context_hit(hit: &Hit) -> context::Hit {
    let p = &hit.passage;
    let modality = Modality::ALL
        .into_iter()
        .find(|m| m.as_str() == p.modality)
        .unwrap_or(Modality::Text);
    context::Hit {
        document_id: p.document_id.clone(),
        unit_id: p.unit_id.clone(),
        passage_id: p.id.clone(),
        modality,
        section_path: p.section_path.clone(),
        page: p.page.map(|v| v as u32),
        start_line: p.start_line.map(|v| v as u32),
        end_line: p.end_line.map(|v| v as u32),
        start_ms: p.start_ms.map(|v| v as u64),
        end_ms: p.end_ms.map(|v| v as u64),
        text: p.text.chars().take(TOOL_OUTPUT_PASSAGE_CHARS).collect(),
        score: hit.score,
        highlight: hit.highlight.clone(),
    }
}

/// Bytes of the JSON a search returning the top `TOOL_SEARCH_K_MAX` of `hits` sends.
pub fn tool_output_bytes(hits: &[Hit]) -> usize {
    let top: Vec<context::Hit> = hits
        .iter()
        .take(TOOL_SEARCH_K_MAX)
        .map(context_hit)
        .collect();
    serde_json::to_string(&top)
        .map(|s| s.len())
        .unwrap_or(usize::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_service::db::PassageRow;
    use margin_service::pipeline::{Diversity, hit_citation};

    fn hit(doc: &str, unit: &str, modality: &str, text: &str) -> Hit {
        Hit {
            passage: PassageRow {
                id: format!("{unit}-{modality}"),
                project_id: String::new(),
                document_id: doc.into(),
                origin: "source".into(),
                scope_ids: vec![],
                access: vec![],
                unit_id: unit.into(),
                modality: modality.into(),
                ord: 0,
                page: None,
                start_line: None,
                end_line: None,
                start_ms: None,
                end_ms: None,
                section_path: String::new(),
                text: text.into(),
                file: None,
                chunk_hash: String::new(),
                embedder: String::new(),
                vector: vec![],
            },
            score: 0.5,
            dense_rank: None,
            lex_rank: None,
            rerank: None,
            highlight: None,
        }
    }

    fn packed(h: Hit, tokens: usize) -> Packed {
        Packed {
            text: h.passage.text.clone(),
            tokens,
            citation: hit_citation(&h),
            hits: vec![h],
        }
    }

    fn budget(tokens: usize, images: usize, k: usize) -> PackBudget {
        PackBudget {
            tokens,
            images_max: images,
            min_score_pct: 25,
            passages_min: 0,
            k,
            diversity: Diversity::default(),
        }
    }

    #[test]
    fn a_good_pack_passes() {
        let p = vec![
            packed(hit("a", "a1", "text", "x"), 100),
            packed(hit("b", "b1", "picture", ""), 1100),
            packed(hit("a", "a2", "text", "y"), 100),
        ];
        assert_eq!(check_packing(&p, &budget(1300, 1, 4)), Vec::<String>::new());
    }

    #[test]
    fn every_breach_is_named() {
        let p = vec![
            packed(hit("a", "a1", "text", "x"), 100),
            packed(hit("a", "a1", "picture", ""), 1100),
            packed(hit("a", "a2", "picture", ""), 1100),
        ];
        let v = check_packing(&p, &budget(1000, 1, 2)).join("\n");
        for want in [
            "2300 tokens packed into a budget of 1000",
            "2 pictures packed, at most 1",
            "3 passages packed, at most k = 2",
            "unit a1 is in passages 1 and 2",
            "3 passages from a, at most 1 per document",
        ] {
            assert!(v.contains(want), "{want} not in:\n{v}");
        }
        let mut no_locator = packed(hit("", "z", "text", "z"), 1);
        no_locator.citation.document_id = None;
        assert!(check_packing(&[no_locator], &budget(10, 0, 4))[0].contains("locator"));
    }

    #[test]
    fn tool_output_is_cut_per_passage() {
        let long = "w".repeat(10_000);
        let hits: Vec<Hit> = (0..20)
            .map(|i| hit("a", &format!("u{i}"), "text", &long))
            .collect();
        let one = context_hit(&hits[0]);
        assert_eq!(one.text.chars().count(), TOOL_OUTPUT_PASSAGE_CHARS);
        let bytes = tool_output_bytes(&hits);
        assert!(bytes > TOOL_SEARCH_K_MAX * TOOL_OUTPUT_PASSAGE_CHARS);
        // The contract's numbers don't fit together: twelve passages cut at 1 200
        // characters plus their locators come to more than the 16 KB a tool output may
        // be, so a search returning full-length chunks breaks the packing gate (and
        // more so with non-ASCII text, where a character is several bytes). Reported to
        // the orchestrator; the gate is what will show it on the corpus.
        assert!(bytes > 16 * 1024, "{bytes}");
        assert!(tool_output_bytes(&hits[..10]) < 16 * 1024);
    }
}
