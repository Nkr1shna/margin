//! The retrieval suite: every golden query through the eval index, each hit judged
//! against the query's anchors, and the §3.2 metrics overall and per query kind.
//!
//! Negatives ("nothing in the corpus answers it") are scored by calibration rather
//! than by a fixed score, because fused RRF scores and cosines of different embedders
//! aren't on one scale: the threshold τ is the 10th percentile of the cosine between
//! answerable queries and their first relevant hit (so nine in ten answerable queries
//! clear it), and a negative is a false positive when any of its top 10 hits clears τ.

use std::collections::BTreeMap;

use margin_service::pipeline::Hit;
use serde::{Deserialize, Serialize};

use crate::golden::{QueryKind, Relevant, RetrievalQuery, matches_anchor};
use crate::index::{EvalIndex, IndexStats, SearchStages, scope_of};
use crate::metrics;

/// The share of answerable queries whose first relevant hit must clear the negatives'
/// threshold.
pub const FP_CALIBRATION_PCT: f64 = 10.0;

/// The indices of `relevant` that `hit` satisfies: same document (and page, when the
/// item names one), and its text contains the anchor or, for an item without an anchor,
/// it is a picture.
pub fn judge(hit: &Hit, relevant: &[Relevant]) -> Vec<usize> {
    let p = &hit.passage;
    relevant
        .iter()
        .enumerate()
        .filter(|(_, r)| {
            r.document == p.document_id
                && r.page.is_none_or(|page| p.page == Some(i64::from(page)))
                && match &r.anchor {
                    Some(anchor) => matches_anchor(&p.text, anchor),
                    None => p.modality == "picture",
                }
        })
        .map(|(i, _)| i)
        .collect()
}

/// Whether each hit overlaps an earlier one: the same unit, or an adjacent text chunk
/// of the same document.
pub fn overlaps(hits: &[Hit]) -> Vec<bool> {
    hits.iter()
        .enumerate()
        .map(|(i, h)| {
            hits[..i].iter().any(|e| {
                e.passage.document_id == h.passage.document_id
                    && (e.passage.unit_id == h.passage.unit_id
                        || (e.passage.modality == "text"
                            && h.passage.modality == "text"
                            && (e.passage.ord - h.passage.ord).abs() <= 1))
            })
        })
        .collect()
}

/// One query's outcome.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueryResult {
    pub id: String,
    pub kind: QueryKind,
    pub n_relevant: usize,
    /// Per hit, best first: the relevant items it satisfies.
    pub judged: Vec<Vec<usize>>,
    pub hit_ids: Vec<String>,
    pub recall_at_5: Option<f64>,
    pub recall_at_10: Option<f64>,
    pub reciprocal_rank: f64,
    pub ndcg_at_10: Option<f64>,
    /// Highest cosine between the query and its top 10 hits.
    pub top_cosine: f32,
    /// Cosine between the query and its first relevant hit in the top 10.
    pub first_relevant_cosine: Option<f32>,
    /// Cross-modal queries: whether a relevant picture is in the top 10.
    pub picture_hit: Option<bool>,
    pub dedup_rate: f64,
    pub ms: f64,
    /// What the rewriter and the reranker did (R2).
    #[serde(default)]
    pub stages: SearchStages,
}

/// What the model-backed stages (R2) did over a run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StageSummary {
    /// The variant's stages and the purpose the suite searched for.
    pub reranker: String,
    pub rewriter: String,
    pub purpose: String,
    /// Points: the mean share of the top 10 places the reranker changed (0 without one).
    pub rerank_moved_at_10: f64,
    /// Points: searches the reranker applies to that it left in the fused order.
    pub rerank_fallback_rate: f64,
    /// Searches the reranker reordered.
    pub reranked: usize,
    pub rerank_ms_p50: f64,
    pub rerank_ms_p95: f64,
    pub rewrite_ms_p50: f64,
    pub rewrite_ms_p95: f64,
    /// Query strings per search after rewriting.
    pub queries_mean: f64,
    /// USD of the small-route calls per search, and in all.
    pub small_cost_usd: f64,
    pub small_cost_usd_total: f64,
    pub small_calls: u64,
}

impl StageSummary {
    /// The numbers of `results` (names left empty).
    pub fn of(results: &[QueryResult]) -> Self {
        let s: Vec<&SearchStages> = results.iter().map(|r| &r.stages).collect();
        let f = |g: &dyn Fn(&SearchStages) -> f64| s.iter().map(|x| g(x)).collect::<Vec<f64>>();
        let moved: Vec<f64> = s.iter().filter_map(|x| x.rerank_moved_at_10).collect();
        let applied = s.iter().filter(|x| x.reranked || x.rerank_fallback).count();
        let fallbacks = s.iter().filter(|x| x.rerank_fallback).count();
        let rerank_ms = f(&|x| x.rerank_ms);
        let rewrite_ms = f(&|x| x.rewrite_ms);
        let cost = f(&|x| x.small_cost_usd);
        Self {
            rerank_moved_at_10: metrics::mean(&moved) * 100.0,
            rerank_fallback_rate: if applied == 0 {
                0.0
            } else {
                fallbacks as f64 * 100.0 / applied as f64
            },
            reranked: s.iter().filter(|x| x.reranked).count(),
            rerank_ms_p50: metrics::percentile(&rerank_ms, 50.0),
            rerank_ms_p95: metrics::percentile(&rerank_ms, 95.0),
            rewrite_ms_p50: metrics::percentile(&rewrite_ms, 50.0),
            rewrite_ms_p95: metrics::percentile(&rewrite_ms, 95.0),
            queries_mean: metrics::mean(&f(&|x| x.queries as f64)),
            small_cost_usd: metrics::mean(&cost),
            small_cost_usd_total: cost.iter().sum(),
            small_calls: s.iter().map(|x| u64::from(x.small_calls)).sum(),
            ..Self::default()
        }
    }

    /// Whether the run had a reranker or a rewriter to report.
    pub fn active(&self) -> bool {
        (!self.reranker.is_empty() && self.reranker != "none")
            || (!self.rewriter.is_empty() && self.rewriter != "none")
    }
}

/// Points (0–100) over a set of queries.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct KindMetrics {
    pub queries: usize,
    pub recall_at_5: f64,
    pub recall_at_10: f64,
    pub mrr: f64,
    pub ndcg_at_10: f64,
}

impl KindMetrics {
    fn of(results: &[&QueryResult]) -> Self {
        let pts = |v: Vec<f64>| metrics::mean(&v) * 100.0;
        let answerable: Vec<&&QueryResult> = results.iter().filter(|r| r.n_relevant > 0).collect();
        Self {
            queries: results.len(),
            recall_at_5: pts(answerable.iter().filter_map(|r| r.recall_at_5).collect()),
            recall_at_10: pts(answerable.iter().filter_map(|r| r.recall_at_10).collect()),
            mrr: pts(answerable.iter().map(|r| r.reciprocal_rank).collect()),
            ndcg_at_10: pts(answerable.iter().filter_map(|r| r.ndcg_at_10).collect()),
        }
    }
}

/// The whole run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RetrievalReport {
    pub embedder_version: String,
    pub pipeline_hash: String,
    pub index: IndexSummary,
    /// Over answerable queries.
    pub overall: KindMetrics,
    pub per_kind: BTreeMap<String, KindMetrics>,
    /// Points: negatives with a hit above the threshold.
    pub negative_fp_rate: f64,
    pub fp_threshold: f32,
    /// Points: cross-modal queries with a relevant picture in the top 10.
    pub picture_hit_rate: f64,
    /// Points: share of top-10 hits overlapping an earlier hit.
    pub dedup_rate: f64,
    pub latency_p50_ms: f64,
    pub latency_p95_ms: f64,
    /// The rewriter and the reranker (R2).
    #[serde(default)]
    pub stages: StageSummary,
    pub queries: Vec<QueryResult>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct IndexSummary {
    pub documents: usize,
    pub text_passages: usize,
    pub picture_passages: usize,
    pub ms: f64,
}

impl From<IndexStats> for IndexSummary {
    fn from(s: IndexStats) -> Self {
        Self {
            documents: s.documents,
            text_passages: s.text_passages,
            picture_passages: s.picture_passages,
            ms: s.ms,
        }
    }
}

pub fn kind_name(kind: QueryKind) -> &'static str {
    match kind {
        QueryKind::Lexical => "lexical",
        QueryKind::Semantic => "semantic",
        QueryKind::CrossModal => "cross-modal",
        QueryKind::Negative => "negative",
    }
}

/// Scores one query's ranked hits.
pub fn score_query(q: &RetrievalQuery, hits: &[Hit], query_vector: &[f32], ms: f64) -> QueryResult {
    let judged: Vec<Vec<usize>> = hits.iter().map(|h| judge(h, &q.relevant)).collect();
    let n = q.relevant.len();
    let top: Vec<&Hit> = hits.iter().take(10).collect();
    let cos = |h: &Hit| margin_embed::cosine(query_vector, &h.passage.vector);
    let top_cosine = top.iter().map(|h| cos(h)).fold(f32::MIN, f32::max);
    let first_relevant_cosine = judged
        .iter()
        .take(10)
        .position(|j| !j.is_empty())
        .map(|i| cos(&hits[i]));
    let picture_hit = (q.kind == QueryKind::CrossModal).then(|| {
        top.iter()
            .zip(&judged)
            .any(|(h, j)| !j.is_empty() && h.passage.modality == "picture")
    });
    QueryResult {
        id: q.id.clone(),
        kind: q.kind,
        n_relevant: n,
        hit_ids: hits.iter().map(|h| h.passage.id.clone()).collect(),
        recall_at_5: metrics::recall_at_k(&judged, n, 5),
        recall_at_10: metrics::recall_at_k(&judged, n, 10),
        reciprocal_rank: metrics::reciprocal_rank(&judged),
        ndcg_at_10: metrics::ndcg_at_k(&judged, n, 10),
        top_cosine: if top.is_empty() { f32::MIN } else { top_cosine },
        first_relevant_cosine,
        picture_hit,
        dedup_rate: metrics::dedup_rate(&overlaps(hits), 10),
        judged,
        ms,
        stages: SearchStages::default(),
    }
}

/// Aggregates per-query results.
pub fn summarize(
    results: Vec<QueryResult>,
    embedder_version: &str,
    pipeline_hash: &str,
    index: IndexSummary,
) -> RetrievalReport {
    let refs: Vec<&QueryResult> = results.iter().collect();
    let answerable: Vec<&QueryResult> = refs.iter().copied().filter(|r| r.n_relevant > 0).collect();
    let mut per_kind = BTreeMap::new();
    for kind in QueryKind::ALL {
        let of: Vec<&QueryResult> = refs.iter().copied().filter(|r| r.kind == kind).collect();
        if !of.is_empty() && kind != QueryKind::Negative {
            per_kind.insert(kind_name(kind).to_string(), KindMetrics::of(&of));
        }
    }
    let calib: Vec<f64> = answerable
        .iter()
        .filter_map(|r| r.first_relevant_cosine.map(f64::from))
        .collect();
    let fp_threshold = if calib.is_empty() {
        f32::INFINITY
    } else {
        metrics::percentile(&calib, FP_CALIBRATION_PCT) as f32
    };
    let negatives: Vec<&QueryResult> = refs.iter().copied().filter(|r| r.n_relevant == 0).collect();
    let fp = negatives
        .iter()
        .filter(|r| metrics::false_positive(&[r.top_cosine], fp_threshold))
        .count();
    let rate = |num: usize, den: usize| {
        if den == 0 {
            0.0
        } else {
            num as f64 * 100.0 / den as f64
        }
    };
    let cross: Vec<bool> = refs.iter().filter_map(|r| r.picture_hit).collect();
    let ms: Vec<f64> = refs.iter().map(|r| r.ms).collect();
    RetrievalReport {
        embedder_version: embedder_version.to_string(),
        pipeline_hash: pipeline_hash.to_string(),
        index,
        overall: KindMetrics::of(&answerable),
        per_kind,
        negative_fp_rate: rate(fp, negatives.len()),
        fp_threshold,
        picture_hit_rate: rate(cross.iter().filter(|b| **b).count(), cross.len()),
        dedup_rate: metrics::mean(&refs.iter().map(|r| r.dedup_rate).collect::<Vec<_>>()) * 100.0,
        latency_p50_ms: metrics::percentile(&ms, 50.0),
        latency_p95_ms: metrics::percentile(&ms, 95.0),
        stages: StageSummary::of(&results),
        queries: results,
    }
}

/// Runs every query through `index`.
pub async fn run(
    index: &EvalIndex<'_>,
    queries: &[RetrievalQuery],
) -> anyhow::Result<RetrievalReport> {
    let mut results = Vec::with_capacity(queries.len());
    for q in queries {
        let ranked = index
            .search(std::slice::from_ref(&q.query), &scope_of(q.scope))
            .await
            .map_err(|e| e.context(format!("query {}", q.id)))?;
        let mut r = score_query(q, &ranked.hits, &ranked.query_vector, ranked.ms);
        r.stages = ranked.stages;
        results.push(r);
    }
    let mut report = summarize(
        results,
        index.vectors.version(),
        &index.pipeline.hash,
        index.stats.into(),
    );
    let cfg = &index.pipeline.config;
    report.stages.reranker = cfg.reranker.name.clone();
    report.stages.rewriter = cfg.rewriter.name.clone();
    report.stages.purpose = index.retrieval_purpose().as_str().to_string();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::{Extraction, documents};
    use crate::golden::{evals_dir, read_jsonl};
    use crate::index::testing::register_stand_ins;
    use crate::recorded::FakeVectors;
    use margin_service::db::{PassageRow, Store};
    use margin_shared::pipeline::PipelineConfig;

    fn hit(doc: &str, unit: &str, modality: &str, ord: i64, page: Option<i64>, text: &str) -> Hit {
        Hit {
            passage: PassageRow {
                id: format!("{doc}|{ord}|{modality}"),
                project_id: String::new(),
                document_id: doc.into(),
                origin: "source".into(),
                scope_ids: vec![],
                access: vec![],
                unit_id: unit.into(),
                modality: modality.into(),
                ord,
                page,
                start_line: None,
                end_line: None,
                start_ms: None,
                end_ms: None,
                section_path: String::new(),
                text: text.into(),
                file: None,
                chunk_hash: String::new(),
                embedder: String::new(),
                vector: vec![1.0, 0.0],
            },
            score: 1.0,
            dense_rank: None,
            lex_rank: None,
            rerank: None,
            highlight: None,
        }
    }

    fn rel(document: &str, page: Option<u32>, anchor: Option<&str>) -> Relevant {
        Relevant {
            document: document.into(),
            page,
            anchor: anchor.map(Into::into),
        }
    }

    #[test]
    fn judging_hits_by_anchor_page_and_picture() {
        let relevant = vec![
            rel("a.md", None, Some("Adding more terms")),
            rel("b.pdf", Some(2), None),
            rel("b.pdf", Some(1), Some("real, inverted")),
        ];
        assert_eq!(
            judge(
                &hit("a.md", "u", "text", 0, None, "x adding  more\nterms y"),
                &relevant
            ),
            vec![0]
        );
        // The anchor in another document doesn't count.
        assert!(
            judge(
                &hit("c.md", "u", "text", 0, None, "Adding more terms"),
                &relevant
            )
            .is_empty()
        );
        // A picture of page 2 satisfies the anchorless item; text of page 2 doesn't.
        assert_eq!(
            judge(&hit("b.pdf", "u", "picture", 2, Some(2), ""), &relevant),
            vec![1]
        );
        assert!(judge(&hit("b.pdf", "u", "text", 2, Some(2), "words"), &relevant).is_empty());
        // The anchored item needs its page.
        assert!(
            judge(
                &hit("b.pdf", "u", "text", 0, Some(2), "a real, inverted image"),
                &relevant
            )
            .is_empty()
        );
        assert_eq!(
            judge(
                &hit("b.pdf", "u", "text", 0, Some(1), "a real, inverted image"),
                &relevant
            ),
            vec![2]
        );
    }

    #[test]
    fn overlapping_neighbours_and_units() {
        let hits = vec![
            hit("a", "a|t3", "text", 3, None, ""),
            hit("a", "a|t4", "text", 4, None, ""),
            hit("a", "a|t9", "text", 9, None, ""),
            hit("b", "b|page1", "text", 0, Some(1), ""),
            hit("b", "b|page1", "picture", 1, Some(1), ""),
            hit("c", "c|t4", "text", 4, None, ""),
        ];
        assert_eq!(
            overlaps(&hits),
            vec![false, true, false, false, true, false]
        );
    }

    #[test]
    fn negatives_are_scored_against_the_calibrated_threshold() {
        let q = |id: &str, kind, frc: Option<f32>, top: f32, n| QueryResult {
            id: id.into(),
            kind,
            n_relevant: n,
            judged: vec![],
            hit_ids: vec![],
            recall_at_5: if n > 0 { Some(1.0) } else { None },
            recall_at_10: if n > 0 { Some(1.0) } else { None },
            reciprocal_rank: if n > 0 { 1.0 } else { 0.0 },
            ndcg_at_10: if n > 0 { Some(1.0) } else { None },
            top_cosine: top,
            first_relevant_cosine: frc,
            picture_hit: None,
            dedup_rate: 0.0,
            ms: 1.0,
            stages: Default::default(),
        };
        let mut results: Vec<QueryResult> = (0..10)
            .map(|i| {
                q(
                    &format!("p{i}"),
                    QueryKind::Semantic,
                    Some(0.5 + i as f32 * 0.05),
                    0.9,
                    1,
                )
            })
            .collect();
        results.push(q("n1", QueryKind::Negative, None, 0.40, 0));
        results.push(q("n2", QueryKind::Negative, None, 0.60, 0));
        let r = summarize(results, "v", "h", IndexSummary::default());
        assert!((r.fp_threshold - 0.5).abs() < 1e-6, "{}", r.fp_threshold);
        assert_eq!(r.negative_fp_rate, 50.0);
        assert_eq!(r.overall.queries, 10);
        assert_eq!(r.overall.recall_at_5, 100.0);
        assert!(!r.per_kind.contains_key("negative"));
    }

    #[tokio::test]
    async fn the_suite_runs_end_to_end_on_stand_ins() {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let ex = Extraction::fixture(&corpus, &docs).unwrap();
        let v = FakeVectors;
        let index = crate::index::EvalIndex::build(
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
        let queries: Vec<RetrievalQuery> =
            read_jsonl(&evals_dir().join("golden/retrieval.jsonl")).unwrap();
        let report = run(&index, &queries).await.unwrap();
        assert_eq!(report.queries.len(), queries.len());
        // Bag of words on lexical queries finds something.
        assert!(
            report.per_kind["lexical"].recall_at_10 > 30.0,
            "{:?}",
            report.per_kind
        );
        assert!(report.latency_p95_ms >= report.latency_p50_ms);
    }
}
