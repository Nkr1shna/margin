//! The eval index: the corpus chunked by a variant's chunker, embedded, written as
//! `passage` rows into a SurrealDB store with the service's real schema and indexes
//! (`margin_service::db`), and searched through the variant's `Pipeline` stages, the
//! same ones the service builds from the same `PipelineConfig`.
//!
//! Nothing here reaches a model: vectors come from a [`Vectors`] (the recording in CI,
//! the real embedder only in the opt-in runner).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use margin_embed::{CHUNK_TOKENS, Purpose, SourceKind};
use margin_service::db::{INSERT_MANY_MAX, PASSAGE, PassageRow, Store};
use margin_service::pipeline::{
    ChunkerCfg, Diversity, FusionCfg, Hit, PackBudget, Packed, Pipeline, Query, Registry,
    StageDeps,
};
use margin_shared::context::{ACCESS_COMPANY, Scope};
use margin_shared::pipeline::PipelineConfig;
use margin_shared::routes::{RETRIEVAL_K_FUSED, RRF_K};
use sha2::{Digest, Sha256};

use crate::corpus::{CorpusDoc, Extraction, approx_tokens};
use crate::golden::GoldenScope;
use crate::recorded::Vectors;

/// The project every eval passage belongs to.
pub const PROJECT_ID: &str = "eval";
/// The topic every corpus document is filed under (`scope_ids`).
pub const CORPUS_SCOPE: &str = "eval-corpus";
/// The workspace the `folder/` tree is attached as (`scope_ids`).
pub const FOLDER_SCOPE: &str = "eval-folder";

/// The retrieval scope of a golden query: the corpus is a topic, the folder tree an
/// attached folder. Retrievers resolve these against `passage.scope_ids` in the index
/// query.
pub fn scope_of(scope: GoldenScope) -> Scope {
    match scope {
        GoldenScope::Corpus => Scope::Topic {
            id: CORPUS_SCOPE.into(),
        },
        GoldenScope::Folder => Scope::Workspace {
            id: FOLDER_SCOPE.into(),
        },
    }
}

/// What indexing wrote.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct IndexStats {
    pub documents: usize,
    pub text_passages: usize,
    pub picture_passages: usize,
    pub ms: f64,
}

/// One search's ranked hits.
#[derive(Debug, Clone)]
pub struct Ranked {
    /// Best first, after fusion and reranking.
    pub hits: Vec<Hit>,
    /// The first query string's vector.
    pub query_vector: Vec<f32>,
    /// Wall time of the retriever legs, fusion and reranking (not query embedding).
    pub ms: f64,
}

pub struct EvalIndex<'v> {
    pub store: Store,
    pub pipeline: Pipeline,
    pub corpus: PathBuf,
    pub vectors: &'v dyn Vectors,
    pub stats: IndexStats,
    docs: HashMap<String, CorpusDoc>,
}

impl<'v> EvalIndex<'v> {
    /// Builds the pipeline `cfg` names from the service's standard registry (plus
    /// whatever `extra` registers, which tests use to fake stages under their names)
    /// and indexes `docs` into `store`.
    pub async fn build(
        store: Store,
        cfg: &PipelineConfig,
        extra: impl FnOnce(&mut Registry),
        vectors: &'v dyn Vectors,
        corpus: &Path,
        docs: &[CorpusDoc],
        extraction: &Extraction,
    ) -> anyhow::Result<Self> {
        let mut reg = Registry::standard(StageDeps {
            store: store.clone(),
        });
        extra(&mut reg);
        let pipeline = Pipeline::build(cfg, &reg)?;
        let mut index = Self {
            store,
            pipeline,
            corpus: corpus.to_path_buf(),
            vectors,
            stats: IndexStats::default(),
            docs: docs.iter().map(|d| (d.path.clone(), d.clone())).collect(),
        };
        let started = Instant::now();
        for doc in docs {
            let rows = index.rows(doc, extraction)?;
            index.stats.documents += 1;
            for r in &rows {
                if r.modality == "picture" {
                    index.stats.picture_passages += 1;
                } else {
                    index.stats.text_passages += 1;
                }
            }
            for batch in rows.chunks(INSERT_MANY_MAX) {
                index
                    .store
                    .insert_many(PASSAGE, batch)
                    .await
                    .with_context(|| format!("couldn't index {}", doc.path))?;
            }
        }
        index.stats.ms = started.elapsed().as_secs_f64() * 1e3;
        Ok(index)
    }

    /// The corpus document a passage belongs to.
    pub fn doc(&self, document_id: &str) -> Option<&CorpusDoc> {
        self.docs.get(document_id)
    }

    /// The chunks of `doc` the pipeline's chunker cuts, as the eval chunks them (the
    /// same token count in recording and replay).
    pub fn chunks(
        &self,
        doc: &CorpusDoc,
        extraction: &Extraction,
    ) -> anyhow::Result<Vec<margin_embed::Chunk>> {
        let ex = extraction.extracted(&self.corpus, doc)?;
        Ok(self.pipeline.chunker.chunk(
            &ex,
            doc.kind,
            &approx_tokens,
            &ChunkerCfg {
                max_tokens: CHUNK_TOKENS,
                params: self.pipeline.config.chunker.params.clone(),
            },
        ))
    }

    fn rows(&self, doc: &CorpusDoc, extraction: &Extraction) -> anyhow::Result<Vec<PassageRow>> {
        let ex = extraction.extracted(&self.corpus, doc)?;
        let chunks = self.chunks(doc, extraction)?;
        let mut scope_ids = vec![CORPUS_SCOPE.to_string()];
        if doc.in_folder {
            scope_ids.push(FOLDER_SCOPE.to_string());
        }
        let unit = |page: Option<u32>, ord: usize| match (doc.kind, page) {
            (SourceKind::Image, _) => format!("{}|image", doc.path),
            (_, Some(p)) => format!("{}|page{p}", doc.path),
            _ => format!("{}|t{ord}", doc.path),
        };
        let base = |id: String, unit_id: String, modality: &str, ord: i64| PassageRow {
            id,
            project_id: PROJECT_ID.into(),
            document_id: doc.path.clone(),
            origin: if doc.in_folder { "folder" } else { "source" }.into(),
            scope_ids: scope_ids.clone(),
            access: vec![ACCESS_COMPANY.into()],
            unit_id,
            modality: modality.into(),
            ord,
            page: None,
            start_line: None,
            end_line: None,
            start_ms: None,
            end_ms: None,
            section_path: String::new(),
            text: String::new(),
            file: None,
            chunk_hash: String::new(),
            embedder: self.vectors.version().to_string(),
            vector: Vec::new(),
        };
        // Embed text chunks grouped by title (the section path, else the file name).
        let title_of = |c: &margin_embed::Chunk| {
            if c.section_path.is_empty() {
                doc.name().to_string()
            } else {
                c.section_path.clone()
            }
        };
        let mut by_title: BTreeMap<String, Vec<usize>> = BTreeMap::new();
        for (i, c) in chunks.iter().enumerate() {
            by_title.entry(title_of(c)).or_default().push(i);
        }
        let mut vectors: Vec<Vec<f32>> = vec![Vec::new(); chunks.len()];
        for (title, idx) in by_title {
            let texts: Vec<String> = idx.iter().map(|i| chunks[*i].text.clone()).collect();
            let vs = self
                .vectors
                .texts(&texts, &Purpose::Document { title: Some(title) })?;
            for (i, v) in idx.into_iter().zip(vs) {
                vectors[i] = v;
            }
        }
        let mut hashes: HashSet<String> = HashSet::new();
        let mut unique = |h: String| {
            let mut h2 = h.clone();
            let mut n = 1;
            while !hashes.insert(h2.clone()) {
                n += 1;
                h2 = format!("{h}#{n}");
            }
            h2
        };
        let mut rows = Vec::new();
        for (ord, (c, v)) in chunks.iter().zip(vectors).enumerate() {
            let mut r = base(
                format!("{}|t{ord}", doc.path),
                unit(c.page, ord),
                "text",
                ord as i64,
            );
            r.page = c.page.map(i64::from);
            r.start_line = c.start_line.map(i64::from);
            r.end_line = c.end_line.map(i64::from);
            r.section_path = c.section_path.clone();
            r.text = c.text.clone();
            r.chunk_hash = unique(if c.hash.is_empty() {
                sha256_hex(&c.text)
            } else {
                c.hash.clone()
            });
            r.vector = v;
            rows.push(r);
        }
        let all_text = ex
            .pages
            .iter()
            .map(|p| p.text.trim())
            .filter(|t| !t.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        for pic in &ex.pictures {
            let text = match pic.page {
                Some(n) => ex
                    .pages
                    .iter()
                    .find(|p| p.number == Some(n))
                    .map(|p| p.text.clone())
                    .unwrap_or_default(),
                None => all_text.clone(),
            };
            let mut r = base(
                format!("{}|p{}", doc.path, pic.page.unwrap_or(0)),
                unit(pic.page, 0),
                "picture",
                pic.page.unwrap_or(0) as i64,
            );
            r.page = pic.page.map(i64::from);
            r.file = Some(match pic.page {
                Some(n) if doc.kind == SourceKind::Pdf => margin_embed::page_file(n),
                _ => doc.name().to_string(),
            });
            r.vector = self.vectors.picture(doc, pic, &text)?;
            r.chunk_hash = unique(sha256_hex(&format!(
                "picture:{}#{:?}",
                doc.path, pic.page
            )));
            r.text = text;
            rows.push(r);
        }
        Ok(rows)
    }

    /// Searches with every string of `plan` (the first is the query proper), fuses the
    /// legs, reranks: what `retrieval::retrieve` does before packing.
    pub async fn search(&self, plan: &[String], scope: &Scope) -> anyhow::Result<Ranked> {
        anyhow::ensure!(!plan.is_empty(), "a search needs a query");
        let vectors = self.vectors.texts(plan, &Purpose::Query)?;
        let started = Instant::now();
        let mut legs = Vec::new();
        for (text, vector) in plan.iter().zip(&vectors) {
            let q = Query {
                text: text.clone(),
                vector: vector.clone(),
                scope: scope.clone(),
                access: None,
                modalities: Vec::new(),
                k: RETRIEVAL_K_FUSED,
                budget_tokens: 0,
                images_max: 0,
                diversity: Diversity::default(),
            };
            legs.push(self.pipeline.retriever.search(&q).await?);
        }
        let fused = self.pipeline.fusion.fuse(
            legs,
            &FusionCfg {
                k_fused: RETRIEVAL_K_FUSED,
                rrf_k: RRF_K,
                params: self.pipeline.config.fusion.params.clone(),
            },
        );
        let hits = self.pipeline.reranker.rerank(&plan[0], fused).await?;
        Ok(Ranked {
            hits,
            query_vector: vectors.into_iter().next().unwrap_or_default(),
            ms: started.elapsed().as_secs_f64() * 1e3,
        })
    }

    /// Packs ranked hits with the pipeline's packer.
    pub fn pack(&self, hits: Vec<Hit>, budget: &PackBudget) -> Vec<Packed> {
        self.pipeline.packer.pack(hits, budget)
    }
}

pub fn sha256_hex(text: &str) -> String {
    hex::encode(Sha256::digest(text.as_bytes()))
}

#[cfg(test)]
pub(crate) mod testing {
    //! Stand-ins registered under the default stage names, so the harness runs end to
    //! end before R1-embed's chunker and R1-retrieve's retriever and packer land: a
    //! brute-force cosine retriever over `passage` rows (scope by `scope_ids`), the
    //! existing page chunker, and a take-k packer with a crude token count.

    use std::sync::Arc;

    use margin_service::engines::BoxFuture;
    use margin_service::pipeline::{
        Chunker, Fusion, Packer, PagesV1, Retriever, StageCtor, TakeK, hit_citation,
    };

    use super::*;

    pub struct BruteRetriever {
        pub store: Store,
    }

    impl Retriever for BruteRetriever {
        fn name(&self) -> &'static str {
            "hybrid-rrf"
        }
        fn search<'a>(&'a self, q: &'a Query) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
            Box::pin(async move {
                let rows: Vec<PassageRow> = match &q.scope {
                    Scope::Topic { id } | Scope::Workspace { id } => {
                        self.store
                            .query_rows(
                                "SELECT *, record::id(id) AS id FROM passage WHERE $scope IN scope_ids",
                                vec![("scope", serde_json::json!(id))],
                            )
                            .await?
                    }
                    Scope::Sources { ids } => {
                        self.store
                            .query_rows(
                                "SELECT *, record::id(id) AS id FROM passage WHERE document_id IN $ids",
                                vec![("ids", serde_json::json!(ids))],
                            )
                            .await?
                    }
                    other => anyhow::bail!("the test retriever doesn't search {other:?}"),
                };
                let mut hits: Vec<Hit> = rows
                    .into_iter()
                    .map(|passage| Hit {
                        score: margin_embed::cosine(&q.vector, &passage.vector),
                        passage,
                        dense_rank: None,
                        lex_rank: None,
                        rerank: None,
                        highlight: None,
                    })
                    .collect();
                hits.sort_by(|a, b| {
                    b.score
                        .total_cmp(&a.score)
                        .then(a.passage.id.cmp(&b.passage.id))
                });
                hits.truncate(q.k);
                for (i, h) in hits.iter_mut().enumerate() {
                    h.dense_rank = Some(i as u32);
                }
                Ok(hits)
            })
        }
    }

    struct FirstLeg;
    impl Fusion for FirstLeg {
        fn name(&self) -> &'static str {
            "rrf"
        }
        fn fuse(&self, legs: Vec<Vec<Hit>>, cfg: &FusionCfg) -> Vec<Hit> {
            let mut hits = legs.into_iter().next().unwrap_or_default();
            hits.truncate(cfg.k_fused);
            hits
        }
    }

    /// Take-k that also fills `tokens`, so the packing checks have something to count.
    struct CountingTakeK;
    impl Packer for CountingTakeK {
        fn name(&self) -> &'static str {
            "mmr-v1"
        }
        fn pack(&self, hits: Vec<Hit>, budget: &PackBudget) -> Vec<Packed> {
            let mut out: Vec<Packed> = TakeK.pack(hits, budget);
            for p in &mut out {
                p.tokens = approx_tokens(&p.text);
                p.citation = hit_citation(&p.hits[0]);
            }
            out
        }
    }

    struct PagesAsSections;
    impl Chunker for PagesAsSections {
        fn name(&self) -> &'static str {
            "sections-v2"
        }
        fn chunk(
            &self,
            doc: &margin_embed::Extracted,
            kind: SourceKind,
            tok: &dyn Fn(&str) -> usize,
            cfg: &ChunkerCfg,
        ) -> Vec<margin_embed::Chunk> {
            PagesV1.chunk(doc, kind, tok, cfg)
        }
    }

    pub fn register_stand_ins(reg: &mut Registry) {
        reg.register(
            "hybrid-rrf",
            StageCtor::Retriever(Arc::new(|_, deps| {
                Ok(Arc::new(BruteRetriever {
                    store: deps.store.clone(),
                }))
            })),
        );
        reg.register("rrf", StageCtor::Fusion(Arc::new(|_, _| Ok(Arc::new(FirstLeg)))));
        reg.register(
            "mmr-v1",
            StageCtor::Packer(Arc::new(|_, _| Ok(Arc::new(CountingTakeK)))),
        );
        reg.register(
            "sections-v2",
            StageCtor::Chunker(Arc::new(|_, _| Ok(Arc::new(PagesAsSections)))),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::corpus::documents;
    use crate::golden::evals_dir;
    use crate::recorded::FakeVectors;

    #[tokio::test]
    async fn indexes_the_corpus_and_searches_by_scope() {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let extraction = Extraction::fixture(&corpus, &docs).unwrap();
        let vectors = FakeVectors;
        let index = EvalIndex::build(
            Store::memory().await.unwrap(),
            &PipelineConfig::default(),
            testing::register_stand_ins,
            &vectors,
            &corpus,
            &docs,
            &extraction,
        )
        .await
        .unwrap();
        assert_eq!(index.stats.documents, docs.len());
        // Two PDFs of 3 + 1 pages… every PDF page and image has its picture row.
        let pictures: usize = extraction.documents.values().map(|d| d.pictures.len()).sum();
        assert_eq!(index.stats.picture_passages, pictures);
        assert!(index.stats.text_passages > 20);

        let ranked = index
            .search(
                &["ablation zone of a glacier".into()],
                &scope_of(GoldenScope::Corpus),
            )
            .await
            .unwrap();
        assert!(ranked.hits[0].passage.text.contains("ablation"));
        assert_eq!(ranked.hits[0].passage.embedder, "fake-bag-of-words");
        // Folder scope sees only the folder's documents.
        let ranked = index
            .search(
                &["RC circuit time constant".into()],
                &scope_of(GoldenScope::Folder),
            )
            .await
            .unwrap();
        assert!(!ranked.hits.is_empty());
        assert!(
            ranked
                .hits
                .iter()
                .all(|h| h.passage.document_id.starts_with("folder/")),
        );
        let doc = index.doc(&ranked.hits[0].passage.document_id).unwrap();
        assert!(doc.in_folder);
        // A page's text and its picture share a unit.
        let rows: Vec<PassageRow> = index
            .store
            .query_rows(
                "SELECT *, record::id(id) AS id FROM passage WHERE document_id = 'pdf/lens-optics.pdf' AND page = 1",
                vec![],
            )
            .await
            .unwrap();
        let units: HashSet<&str> = rows.iter().map(|r| r.unit_id.as_str()).collect();
        assert_eq!(units.len(), 1, "{rows:?}");
        assert!(rows.iter().any(|r| r.modality == "picture"));
        assert!(rows.iter().any(|r| r.modality == "text"));
    }

    #[tokio::test]
    async fn an_unrecorded_document_fails_the_build() {
        let corpus = evals_dir().join("corpus");
        let docs = documents(&corpus).unwrap();
        let vectors = FakeVectors;
        let err = EvalIndex::build(
            Store::memory().await.unwrap(),
            &PipelineConfig::default(),
            testing::register_stand_ins,
            &vectors,
            &corpus,
            &docs,
            &Extraction::default(),
        )
        .await
        .err()
        .unwrap();
        assert!(err.to_string().contains("no recorded extraction"), "{err}");
    }
}
