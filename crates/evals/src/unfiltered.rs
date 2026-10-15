//! The access suite's replay without the reader (L2 D8, `owner_rank_identical`): the
//! service's index retrievers' statements (`retrieval::IndexRetriever`: `hybrid-rrf`,
//! `hybrid-linear`, `dense`, `bm25`) as they were before L2, with no
//! `access CONTAINSANY $paths AND deny CONTAINSNONE $deny`. Test-only by construction:
//! it lives in the eval crate, never in the service, and only the access suite builds it.
//!
//! It mirrors the service's statements on purpose. If they drift apart, the owner's
//! ranked ids stop matching and the gate fails loudly; it never passes wrongly.

use std::collections::HashMap;

use margin_service::db::{PassageRow, Store};
use margin_service::engines::BoxFuture;
use margin_service::pipeline::{Hit, Query, Retriever};
use margin_service::retrieval::{HIGHLIGHT_CLOSE, HIGHLIGHT_OPEN, resolve_for_index};
use margin_shared::pipeline::StageCfg;
use margin_shared::routes::{RETRIEVAL_K_DENSE, RETRIEVAL_K_LEX, RRF_K};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Legs {
    Hybrid,
    HybridLinear,
    Dense,
    Lexical,
}

/// The retriever a variant names, without the reader's two conjuncts.
pub struct UnfilteredRetriever {
    store: Store,
    legs: Legs,
    k_dense: usize,
    k_lex: usize,
    rrf_k: u32,
    ef: usize,
    dense_weight: f32,
    lex_weight: f32,
}

#[derive(Debug, Deserialize)]
struct RankedRow {
    #[serde(flatten)]
    row: PassageRow,
    #[serde(default)]
    d: Option<f64>,
    #[serde(default)]
    s: Option<f64>,
    #[serde(default)]
    highlight: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct FusedRef {
    pid: String,
    rrf_score: f64,
    #[serde(default)]
    highlight: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct HybridOut {
    #[serde(default)]
    fused: Vec<FusedRef>,
    #[serde(default)]
    dense: Vec<String>,
    #[serde(default)]
    lex: Vec<String>,
    #[serde(default)]
    rows: Vec<PassageRow>,
}

fn highlight_text(v: Option<Value>) -> Option<String> {
    match v? {
        Value::String(s) if !s.is_empty() => Some(s),
        _ => None,
    }
}

/// The scope and modality conditions (`retrieval::Filter` without the reader).
struct Cond {
    library: bool,
    keys: Vec<String>,
    modalities: Vec<String>,
    embedder: String,
}

impl Cond {
    fn sql(&self) -> String {
        let mut sql = String::new();
        if !self.library {
            sql.push_str(" AND (scope_ids CONTAINSANY $keys OR document_id IN $keys)");
        }
        if !self.modalities.is_empty() {
            sql.push_str(" AND modality IN $modalities");
        }
        sql
    }

    fn dense_sql(&self) -> String {
        let mut sql = self.sql();
        if !self.embedder.is_empty() {
            sql.push_str(" AND embedder = $embedder");
        }
        sql
    }

    fn binds(&self) -> Vec<(&'static str, Value)> {
        let mut binds = vec![
            ("keys", json!(self.keys)),
            ("modalities", json!(self.modalities)),
        ];
        if !self.embedder.is_empty() {
            binds.push(("embedder", json!(self.embedder)));
        }
        binds
    }
}

impl UnfilteredRetriever {
    /// The unfiltered twin of the retriever `cfg` names, with its params.
    pub fn new(store: Store, cfg: &StageCfg) -> anyhow::Result<Self> {
        let legs = match cfg.name.as_str() {
            "hybrid-rrf" => Legs::Hybrid,
            "hybrid-linear" => Legs::HybridLinear,
            "dense" => Legs::Dense,
            "bm25" => Legs::Lexical,
            other => anyhow::bail!(
                "the access suite's unfiltered replay knows the index retrievers \
                 (hybrid-rrf, hybrid-linear, dense, bm25), not {other}"
            ),
        };
        let p = &cfg.params;
        let usize_of = |key: &str, default: usize| {
            p.get(key)
                .and_then(Value::as_u64)
                .map_or(default, |v| v as usize)
        };
        let f32_of = |key: &str, default: f32| {
            p.get(key)
                .and_then(Value::as_f64)
                .map_or(default, |v| v as f32)
        };
        Ok(Self {
            store,
            legs,
            k_dense: usize_of("k_dense", RETRIEVAL_K_DENSE).max(1),
            k_lex: usize_of("k_lex", RETRIEVAL_K_LEX).max(1),
            rrf_k: usize_of("rrf_k", RRF_K as usize) as u32,
            ef: usize_of("ef", 128).max(1),
            dense_weight: f32_of("dense_weight", 1.0),
            lex_weight: f32_of("lex_weight", 1.0),
        })
    }

    fn dense_sql(&self, c: &Cond, k: usize, fields: &str) -> String {
        format!(
            "SELECT {fields}, vector::distance::knn() AS d FROM passage \
             WHERE vector <|{k},{ef}|> $q{cond} ORDER BY d",
            ef = self.ef.max(k),
            cond = c.dense_sql(),
        )
    }

    fn lex_sql(&self, c: &Cond, k: usize, fields: &str) -> String {
        format!(
            "SELECT {fields}, search::score(1) AS s, \
             search::highlight('{HIGHLIGHT_OPEN}', '{HIGHLIGHT_CLOSE}', 1) AS highlight \
             FROM passage WHERE text @1@ $text{cond} ORDER BY s DESC LIMIT {k}",
            cond = c.sql(),
        )
    }

    async fn dense(&self, c: &Cond, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let sql = self.dense_sql(c, self.k_dense, "*, record::id(id) AS id");
        let mut binds = c.binds();
        binds.push(("q", json!(q.vector)));
        let rows: Vec<RankedRow> = self.store.query_rows(&sql, binds).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| Hit {
                score: 1.0 - r.d.unwrap_or(1.0) as f32,
                passage: r.row,
                dense_rank: Some(i as u32),
                lex_rank: None,
                rerank: None,
                highlight: None,
            })
            .collect())
    }

    async fn lexical(&self, c: &Cond, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let sql = self.lex_sql(c, self.k_lex, "*, record::id(id) AS id");
        let mut binds = c.binds();
        binds.push(("text", json!(q.text)));
        let rows: Vec<RankedRow> = self.store.query_rows(&sql, binds).await?;
        Ok(rows
            .into_iter()
            .enumerate()
            .map(|(i, r)| Hit {
                score: r.s.unwrap_or(0.0) as f32,
                passage: r.row,
                dense_rank: None,
                lex_rank: Some(i as u32),
                rerank: None,
                highlight: highlight_text(r.highlight),
            })
            .collect())
    }

    async fn hybrid(&self, c: &Cond, q: &Query, k_fused: usize) -> anyhow::Result<Vec<Hit>> {
        let fields = "id, record::id(id) AS pid";
        let sql = format!(
            "LET $dense = {dense}; LET $lex = {lex}; \
             LET $fused = search::rrf([$dense, $lex], {k_fused}, {rrf_k}); \
             RETURN {{ fused: (SELECT pid, rrf_score, highlight FROM $fused), \
               dense: $dense.pid, lex: $lex.pid, \
               rows: (SELECT *, record::id(id) AS id FROM passage WHERE id IN $fused.id) }};",
            dense = self.dense_sql(c, self.k_dense, fields),
            lex = self.lex_sql(c, self.k_lex, fields),
            rrf_k = self.rrf_k,
        );
        let mut binds = c.binds();
        binds.push(("q", json!(q.vector)));
        binds.push(("text", json!(q.text)));
        let out: Vec<HybridOut> = self.store.query_rows(&sql, binds).await?;
        let Some(out) = out.into_iter().next() else {
            return Ok(Vec::new());
        };
        let mut rows: HashMap<String, PassageRow> =
            out.rows.into_iter().map(|r| (r.id.clone(), r)).collect();
        let rank = |list: &[String], id: &str| list.iter().position(|x| x == id).map(|i| i as u32);
        Ok(out
            .fused
            .into_iter()
            .filter_map(|f| {
                let passage = rows.remove(&f.pid)?;
                Some(Hit {
                    score: f.rrf_score as f32,
                    dense_rank: rank(&out.dense, &f.pid),
                    lex_rank: rank(&out.lex, &f.pid),
                    rerank: None,
                    highlight: highlight_text(f.highlight),
                    passage,
                })
            })
            .collect())
    }

    async fn hybrid_linear(&self, c: &Cond, q: &Query) -> anyhow::Result<Vec<Hit>> {
        let dense = self.dense(c, q).await?;
        let lex = self.lexical(c, q).await?;
        let normalised = |leg: &[Hit]| -> Vec<f32> {
            let (lo, hi) = leg.iter().fold((f32::MAX, f32::MIN), |(lo, hi), h| {
                (lo.min(h.score), hi.max(h.score))
            });
            leg.iter()
                .map(|h| {
                    if hi > lo {
                        (h.score - lo) / (hi - lo)
                    } else {
                        1.0
                    }
                })
                .collect()
        };
        let (dn, ln) = (normalised(&dense), normalised(&lex));
        let mut fused: Vec<Hit> = Vec::new();
        let mut at: HashMap<String, usize> = HashMap::new();
        for (hit, n) in dense.into_iter().zip(dn) {
            at.insert(hit.passage.id.clone(), fused.len());
            fused.push(Hit {
                score: self.dense_weight * n,
                ..hit
            });
        }
        for (hit, n) in lex.into_iter().zip(ln) {
            match at.get(&hit.passage.id) {
                Some(&i) => {
                    let f = &mut fused[i];
                    f.score += self.lex_weight * n;
                    f.lex_rank = hit.lex_rank;
                    f.highlight = hit.highlight;
                }
                None => fused.push(Hit {
                    score: self.lex_weight * n,
                    ..hit
                }),
            }
        }
        fused.sort_by(|a, b| b.score.total_cmp(&a.score));
        Ok(fused)
    }
}

impl Retriever for UnfilteredRetriever {
    fn name(&self) -> &'static str {
        "unfiltered"
    }

    fn search<'a>(&'a self, q: &'a Query) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        Box::pin(async move {
            let resolved = resolve_for_index(&self.store, &q.scope).await?;
            if resolved.is_empty() {
                return Ok(Vec::new());
            }
            let c = Cond {
                library: resolved.library,
                keys: resolved.keys,
                modalities: q
                    .modalities
                    .iter()
                    .map(|m| m.as_str().to_string())
                    .collect(),
                embedder: q.embedder.clone(),
            };
            let has_vector = !q.vector.is_empty();
            let has_text = !q.text.trim().is_empty();
            let k = q.k.max(1);
            let mut hits = match (self.legs, has_vector, has_text) {
                (Legs::Hybrid, true, true) => self.hybrid(&c, q, k).await?,
                (Legs::HybridLinear, true, true) => self.hybrid_linear(&c, q).await?,
                (Legs::Hybrid | Legs::HybridLinear | Legs::Dense, true, _) => {
                    self.dense(&c, q).await?
                }
                (Legs::Hybrid | Legs::HybridLinear | Legs::Lexical, _, true) => {
                    self.lexical(&c, q).await?
                }
                _ => Vec::new(),
            };
            hits.truncate(k);
            Ok(hits)
        })
    }
}
