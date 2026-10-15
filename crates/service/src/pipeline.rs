//! The retrieval and generation pipeline as named stages behind traits, built from
//! configuration (`docs/agent-rag-architecture.md` §2.0).
//!
//! Frozen after R1's first commit: the stage traits, `Query`, `Hit`, `Packed`,
//! `PackBudget`, `Prompt` and `Registry`'s API. Stage interiors are the areas' to fill:
//! each area registers its variants by name from its own file (`register` in
//! `ingest.rs` and `retrieval.rs`), and `Pipeline::build` refuses a config naming a
//! stage that isn't registered. The service, the eval runner and the tests build the
//! same `Pipeline` from the same `PipelineConfig`.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Context;
use margin_ai::LessonContext;
use margin_embed::{Chunk, Extracted, SourceKind};
use margin_shared::Citation;
use margin_shared::context::{Modality, Scope};
use margin_shared::pipeline::{PipelineConfig, RouteTable, StageCfg};

use crate::db::{PassageRow, Store};
use crate::engines::BoxFuture;

// ---------------------------------------------------------------------------
// What flows between stages
// ---------------------------------------------------------------------------

/// A chunker's knobs: the hard token cap plus the variant's own parameters.
#[derive(Debug, Clone)]
pub struct ChunkerCfg {
    /// No chunk may count more tokens than this (`margin_embed::CHUNK_TOKENS`).
    pub max_tokens: usize,
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// One retrieval request, after query planning: one query string with its vector.
#[derive(Debug, Clone)]
pub struct Query {
    pub text: String,
    /// The query's embedding (`Purpose::Query`); empty for a lexical-only search.
    pub vector: Vec<f32>,
    /// The version of the embedder that made `vector` (`Embedder::version`): the dense
    /// leg compares it only with passages of that version (`embedder = $embedder`), so a
    /// half re-embedded index never mixes vector spaces. Empty = no such filter.
    pub embedder: String,
    pub scope: Scope,
    /// The caller's access paths; `None` = no filter (today's single owner). Retrievers
    /// must apply it inside the index query (pre-filter), never to their results.
    pub access: Option<Vec<String>>,
    /// Empty means every modality.
    pub modalities: Vec<Modality>,
    /// Candidates wanted from this stage.
    pub k: usize,
    pub budget_tokens: usize,
    pub images_max: usize,
    pub diversity: Diversity,
}

/// How the packer spreads its picks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Diversity {
    /// MMR's relevance weight (1.0 = relevance only).
    pub lambda: f32,
    /// Most passages from one document; `None` lets the packer decide (`ceil(k / 2)`).
    pub per_document_max: Option<usize>,
}

impl Default for Diversity {
    fn default() -> Self {
        Self {
            lambda: margin_shared::routes::MMR_LAMBDA,
            per_document_max: None,
        }
    }
}

/// A retrieved passage and how it ranked in each leg.
#[derive(Debug, Clone, PartialEq)]
pub struct Hit {
    pub passage: PassageRow,
    /// The fused (or only) score; higher is better.
    pub score: f32,
    pub dense_rank: Option<u32>,
    pub lex_rank: Option<u32>,
    pub rerank: Option<f32>,
    pub highlight: Option<String>,
}

/// The fusion stage's knobs.
#[derive(Debug, Clone)]
pub struct FusionCfg {
    /// Candidates kept after fusing (`routes::RETRIEVAL_K_FUSED`).
    pub k_fused: usize,
    /// RRF's `k` (`routes::RRF_K`).
    pub rrf_k: u32,
    pub params: serde_json::Map<String, serde_json::Value>,
}

/// The budget a packer fills, resolved from `margin_shared::pipeline::Budget` against
/// the route's model window.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PackBudget {
    /// Tokens of passages (pictures counted at their provider cost) that fit.
    pub tokens: usize,
    pub images_max: usize,
    /// Hits under this percentile of the top list's score are dropped.
    pub min_score_pct: u8,
    pub passages_min: usize,
    /// Most passages, whatever the token budget (`k` of the request).
    pub k: usize,
    pub diversity: Diversity,
}

/// A passage ready for a prompt: possibly several adjacent hits merged, with the
/// citation that points back at it.
#[derive(Debug, Clone, PartialEq)]
pub struct Packed {
    /// The hits it was made from, best first; the first one's locator is the citation's.
    pub hits: Vec<Hit>,
    pub text: String,
    /// Tokens it costs in the prompt (text, plus the picture's provider cost).
    pub tokens: usize,
    /// `n` is filled in when a body cites it; `image_url` holds the bare media file name.
    pub citation: Citation,
}

/// A prompt laid out for caching (§2.6): the constant part first, byte-identical across
/// calls of one `version`, then the variable part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prompt {
    pub version: String,
    pub constant: String,
    pub variable: String,
    /// `prompt_cache_key` for OpenAI; Claude breakpoints go after `constant`.
    pub cache_key: String,
}

/// What the outline planner is given (§2.5).
#[derive(Debug, Clone, Default)]
pub struct PlanInput {
    pub request: String,
    /// Each document's name and section paths: a table of contents.
    pub contents: Vec<String>,
    pub survey: Vec<Packed>,
}

// ---------------------------------------------------------------------------
// The stages
// ---------------------------------------------------------------------------

pub trait Chunker: Send + Sync {
    fn name(&self) -> &'static str;
    fn chunk(
        &self,
        doc: &Extracted,
        kind: SourceKind,
        tok: &dyn Fn(&str) -> usize,
        cfg: &ChunkerCfg,
    ) -> Vec<Chunk>;
}

pub trait Retriever: Send + Sync {
    fn name(&self) -> &'static str;
    fn search<'a>(&'a self, q: &'a Query) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>>;
}

pub trait Fusion: Send + Sync {
    fn name(&self) -> &'static str;
    fn fuse(&self, legs: Vec<Vec<Hit>>, cfg: &FusionCfg) -> Vec<Hit>;
}

pub trait Reranker: Send + Sync {
    fn name(&self) -> &'static str;
    fn rerank<'a>(&'a self, q: &'a str, hits: Vec<Hit>) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>>;
}

pub trait Packer: Send + Sync {
    fn name(&self) -> &'static str;
    fn pack(&self, hits: Vec<Hit>, budget: &PackBudget) -> Vec<Packed>;
}

pub trait PromptSet: Send + Sync {
    fn version(&self) -> &'static str;
    fn lesson(&self, ctx: &LessonContext) -> Prompt;
    fn plan(&self, input: &PlanInput) -> Prompt;
    fn rerank(&self, query: &str, hits: &[Hit]) -> Prompt;
}

// ---------------------------------------------------------------------------
// Registry and Pipeline
// ---------------------------------------------------------------------------

/// What stage constructors may use.
#[derive(Clone)]
pub struct StageDeps {
    pub store: Store,
}

type Ctor<T> = Arc<dyn Fn(&StageCfg, &StageDeps) -> anyhow::Result<Arc<T>> + Send + Sync>;

/// A constructor for one kind of stage.
pub enum StageCtor {
    Chunker(Ctor<dyn Chunker>),
    Retriever(Ctor<dyn Retriever>),
    Fusion(Ctor<dyn Fusion>),
    Reranker(Ctor<dyn Reranker>),
    Packer(Ctor<dyn Packer>),
    Prompts(Ctor<dyn PromptSet>),
}

impl StageCtor {
    fn kind(&self) -> &'static str {
        match self {
            StageCtor::Chunker(_) => "chunker",
            StageCtor::Retriever(_) => "retriever",
            StageCtor::Fusion(_) => "fusion",
            StageCtor::Reranker(_) => "reranker",
            StageCtor::Packer(_) => "packer",
            StageCtor::Prompts(_) => "prompts",
        }
    }
}

/// Stage constructors by kind and name. Tests register fakes under the names they fake.
pub struct Registry {
    deps: StageDeps,
    ctors: HashMap<(&'static str, String), StageCtor>,
    /// The `embedder` stage's known names. The stage builds nothing here: it names whose
    /// vectors a pipeline is measured with (the service embeds with `service.toml`'s
    /// `embedder`; the eval runner loads the backend a variant names).
    embedders: std::collections::BTreeSet<String>,
}

/// The `embedder` names besides the backends: the eval runner's recorded vectors and
/// the test fake.
pub const EMBEDDER_RECORDED: &str = "recorded";
pub const EMBEDDER_FAKE: &str = "fake";
/// The stage a config without an `embedder` names (`margin_shared::pipeline::
/// DEFAULT_EMBEDDER`, `"coreml"`): the Mac service's Core ML backend, which no build runs
/// any more. Still known so every stored config and `pipeline_hash` keeps its meaning;
/// the service stamps its own backend on the pipeline it runs.
pub const EMBEDDER_LEGACY_DEFAULT: &str = margin_shared::pipeline::DEFAULT_EMBEDDER;

impl Registry {
    /// An empty registry.
    pub fn new(deps: StageDeps) -> Self {
        Self {
            deps,
            ctors: HashMap::new(),
            embedders: Default::default(),
        }
    }

    /// Every variant the service ships: the trivial ones here, the rest from the areas'
    /// files, and every embedding backend (`margin_embed::Backend`) plus the legacy
    /// default, since a pipeline names the vectors it's measured with.
    pub fn standard(deps: StageDeps) -> Self {
        let mut reg = Self::new(deps);
        register_trivial(&mut reg);
        crate::ingest::register(&mut reg);
        crate::retrieval::register(&mut reg);
        for b in margin_embed::Backend::ALL {
            reg.register_embedder(b.name());
        }
        reg.register_embedder(EMBEDDER_LEGACY_DEFAULT);
        reg.register_embedder(EMBEDDER_RECORDED);
        reg.register_embedder(EMBEDDER_FAKE);
        reg
    }

    /// Adds (or replaces) the constructor of `name` for its kind of stage.
    pub fn register(&mut self, name: &str, ctor: StageCtor) {
        self.ctors.insert((ctor.kind(), name.to_string()), ctor);
    }

    /// Adds `name` to the `embedder` stage's known names.
    pub fn register_embedder(&mut self, name: &str) {
        self.embedders.insert(name.to_string());
    }

    /// The registered names of one kind of stage ("chunker", "retriever", …,
    /// "embedder"), sorted.
    pub fn names(&self, kind: &str) -> Vec<String> {
        if kind == "embedder" {
            return self.embedders.iter().cloned().collect();
        }
        let mut names: Vec<String> = self
            .ctors
            .keys()
            .filter(|(k, _)| *k == kind)
            .map(|(_, n)| n.clone())
            .collect();
        names.sort();
        names
    }

    /// An error naming the stage when `cfg` isn't a known embedder.
    fn check_embedder(&self, cfg: &StageCfg) -> anyhow::Result<()> {
        if self.embedders.contains(&cfg.name) {
            return Ok(());
        }
        anyhow::bail!(
            "the pipeline's embedder \"{}\" isn't a known embedder (known: {})",
            cfg.name,
            self.names("embedder").join(", ")
        )
    }

    fn get(&self, kind: &'static str, cfg: &StageCfg) -> anyhow::Result<&StageCtor> {
        self.ctors.get(&(kind, cfg.name.clone())).with_context(|| {
            format!(
                "the pipeline's {kind} \"{}\" isn't a known {kind} (known: {})",
                cfg.name,
                self.names(kind).join(", ")
            )
        })
    }
}

/// The stages a config names, built.
pub struct Pipeline {
    pub config: PipelineConfig,
    /// `config.hash()`, stamped on traces, proposals, lessons and eval runs.
    pub hash: String,
    pub chunker: Arc<dyn Chunker>,
    pub retriever: Arc<dyn Retriever>,
    pub fusion: Arc<dyn Fusion>,
    pub reranker: Arc<dyn Reranker>,
    pub packer: Arc<dyn Packer>,
    pub prompts: Arc<dyn PromptSet>,
    pub routes: RouteTable,
}

impl Pipeline {
    /// Builds every stage `cfg` names; an unknown name is an error naming the stage.
    pub fn build(cfg: &PipelineConfig, reg: &Registry) -> anyhow::Result<Self> {
        let deps = &reg.deps;
        reg.check_embedder(&cfg.embedder)?;
        let prompts_cfg = StageCfg::named(&cfg.prompts);
        macro_rules! stage {
            ($kind:literal, $variant:ident, $cfg:expr) => {
                match reg.get($kind, $cfg)? {
                    StageCtor::$variant(ctor) => ctor($cfg, deps).with_context(|| {
                        format!("couldn't build the {} \"{}\"", $kind, $cfg.name)
                    })?,
                    _ => unreachable!("registered under its own kind"),
                }
            };
        }
        Ok(Self {
            config: cfg.clone(),
            hash: cfg.hash(),
            chunker: stage!("chunker", Chunker, &cfg.chunker),
            retriever: stage!("retriever", Retriever, &cfg.retriever),
            fusion: stage!("fusion", Fusion, &cfg.fusion),
            reranker: stage!("reranker", Reranker, &cfg.reranker),
            packer: stage!("packer", Packer, &cfg.packer),
            prompts: stage!("prompts", Prompts, &prompts_cfg),
            routes: cfg.routes.clone(),
        })
    }
}

// ---------------------------------------------------------------------------
// Trivial variants, so everything builds before the areas land
// ---------------------------------------------------------------------------

fn register_trivial(reg: &mut Registry) {
    reg.register(
        "pages-v1",
        StageCtor::Chunker(Arc::new(|_, _| Ok(Arc::new(PagesV1)))),
    );
    reg.register(
        "first",
        StageCtor::Fusion(Arc::new(|_, _| Ok(Arc::new(First)))),
    );
    reg.register(
        "none",
        StageCtor::Reranker(Arc::new(|_, _| Ok(Arc::new(NoRerank)))),
    );
    reg.register(
        "take-k",
        StageCtor::Packer(Arc::new(|_, _| Ok(Arc::new(TakeK)))),
    );
    reg.register(
        "lesson-v2",
        StageCtor::Prompts(Arc::new(|_, _| Ok(Arc::new(LessonV2)))),
    );
}

/// Today's chunker: `margin_embed::chunk` (page- and sentence-aware, no headings).
pub struct PagesV1;

impl Chunker for PagesV1 {
    fn name(&self) -> &'static str {
        "pages-v1"
    }
    fn chunk(
        &self,
        doc: &Extracted,
        kind: SourceKind,
        tok: &dyn Fn(&str) -> usize,
        cfg: &ChunkerCfg,
    ) -> Vec<Chunk> {
        margin_embed::chunk(doc, kind, tok, cfg.max_tokens)
    }
}

/// Takes the first leg as it is.
pub struct First;

impl Fusion for First {
    fn name(&self) -> &'static str {
        "first"
    }
    fn fuse(&self, legs: Vec<Vec<Hit>>, cfg: &FusionCfg) -> Vec<Hit> {
        let mut hits = legs.into_iter().next().unwrap_or_default();
        hits.truncate(cfg.k_fused);
        hits
    }
}

/// Keeps the order it is given.
pub struct NoRerank;

impl Reranker for NoRerank {
    fn name(&self) -> &'static str {
        "none"
    }
    fn rerank<'a>(
        &'a self,
        _q: &'a str,
        hits: Vec<Hit>,
    ) -> BoxFuture<'a, anyhow::Result<Vec<Hit>>> {
        Box::pin(async move { Ok(hits) })
    }
}

/// The first `k` hits, each its own passage, no budget or dedup.
pub struct TakeK;

/// The section a hit is in ("Chapter 2 > Fourier series"), for its citation; `None`
/// when the chunker named none.
pub fn citation_section_path(hit: &Hit) -> Option<String> {
    let s = hit.passage.section_path.trim();
    (!s.is_empty()).then(|| s.to_string())
}

/// The citation of one hit (`n` 0 until cited; `source_id` is the document id).
pub fn hit_citation(hit: &Hit) -> Citation {
    let p = &hit.passage;
    Citation {
        n: 0,
        source_id: p.document_id.clone(),
        source_name: String::new(),
        kind: if p.page.is_some() {
            SourceKind::Pdf
        } else {
            SourceKind::Text
        },
        page: p.page.map(|v| v as u32),
        start_ms: p.start_ms.map(|v| v as u64),
        end_ms: p.end_ms.map(|v| v as u64),
        excerpt: p.text.chars().take(2_000).collect(),
        image_url: p.file.clone(),
        document_id: Some(p.document_id.clone()),
        start_line: p.start_line.map(|v| v as u32),
        end_line: p.end_line.map(|v| v as u32),
        file: None,
        section_path: citation_section_path(hit),
    }
}

impl Packer for TakeK {
    fn name(&self) -> &'static str {
        "take-k"
    }
    fn pack(&self, hits: Vec<Hit>, budget: &PackBudget) -> Vec<Packed> {
        hits.into_iter()
            .take(budget.k)
            .map(|hit| Packed {
                text: hit.passage.text.clone(),
                tokens: 0,
                citation: hit_citation(&hit),
                hits: vec![hit],
            })
            .collect()
    }
}

/// The constant-first lesson, planner and rerank prompts of `margin_ai::prompts`
/// (§2.6), laid out as the pipeline's `Prompt`: the request and passages follow the
/// variable instructions (pictures are left out of this text form).
pub struct LessonV2;

fn from_ai(p: margin_ai::Prompt) -> Prompt {
    let cache_key = p.cache_key();
    let input = p.input_text();
    let variable = match (p.variable.is_empty(), input.is_empty()) {
        (_, true) => p.variable,
        (true, false) => input,
        (false, false) => format!("{}\n\n{input}", p.variable),
    };
    Prompt {
        version: p.version.to_string(),
        constant: p.constant.to_string(),
        variable,
        cache_key,
    }
}

/// A packed passage as the prompts number it.
fn packed_passage(p: &Packed) -> margin_ai::Passage {
    margin_ai::Passage {
        source: p.citation.source_name.clone(),
        page: p.citation.page,
        text: p.text.clone(),
        start_ms: p.citation.start_ms,
        end_ms: p.citation.end_ms,
        image: None,
    }
}

impl PromptSet for LessonV2 {
    fn version(&self) -> &'static str {
        margin_ai::prompts::PROMPT_SET_VERSION
    }
    fn lesson(&self, ctx: &LessonContext) -> Prompt {
        from_ai(margin_ai::prompts::lesson(ctx))
    }
    /// `PlanInput` carries the course's options inside `request` (the service builds a
    /// full `margin_ai::PlanRequest` itself, `ask::survey`); here they keep the prompt's
    /// defaults.
    fn plan(&self, input: &PlanInput) -> Prompt {
        let req = margin_ai::PlanRequest {
            prompt: input.request.clone(),
            level: margin_shared::Level::Intermediate,
            lesson_count: 0,
            with_quiz: false,
            draft: None,
            contents: input
                .contents
                .iter()
                .map(|c| margin_ai::DocumentContents {
                    document: c.clone(),
                    sections: Vec::new(),
                })
                .collect(),
            survey: input.survey.iter().map(packed_passage).collect(),
        };
        from_ai(margin_ai::prompts::plan(&req))
    }
    fn rerank(&self, query: &str, hits: &[Hit]) -> Prompt {
        let candidates: Vec<margin_ai::RerankCandidate> = hits
            .iter()
            .map(|h| margin_ai::RerankCandidate {
                id: h.passage.id.clone(),
                label: if h.passage.section_path.is_empty() {
                    h.passage.document_id.clone()
                } else {
                    format!("{}, {}", h.passage.document_id, h.passage.section_path)
                },
                text: h.passage.text.clone(),
            })
            .collect();
        from_ai(margin_ai::prompts::rerank(query, &candidates))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn registry() -> Registry {
        Registry::standard(StageDeps {
            store: Store::memory().await.unwrap(),
        })
    }

    fn trivial() -> PipelineConfig {
        PipelineConfig {
            chunker: StageCfg::named("pages-v1"),
            retriever: StageCfg::named("dense"),
            fusion: StageCfg::named("first"),
            reranker: StageCfg::named("none"),
            packer: StageCfg::named("take-k"),
            ..PipelineConfig::default()
        }
    }

    #[tokio::test]
    async fn the_default_and_the_trivial_configs_build() {
        let reg = registry().await;
        let p = Pipeline::build(&PipelineConfig::default(), &reg).unwrap();
        assert_eq!(p.chunker.name(), "sections-v2");
        assert_eq!(p.retriever.name(), "hybrid-rrf");
        assert_eq!(p.fusion.name(), "rrf");
        assert_eq!(p.reranker.name(), "none");
        assert_eq!(p.packer.name(), "mmr-v1");
        assert_eq!(p.prompts.version(), "lesson-v2");
        assert_eq!(p.hash, PipelineConfig::default().hash());
        let p = Pipeline::build(&trivial(), &reg).unwrap();
        assert_eq!(p.retriever.name(), "dense");
        assert_eq!(p.packer.name(), "take-k");
    }

    #[tokio::test]
    async fn the_embedder_stage_must_be_a_known_name() {
        let reg = registry().await;
        let names = reg.names("embedder");
        for n in [
            "coreml",
            "onnx-q8",
            "onnx-fp32",
            "onnx-fp16",
            "recorded",
            "fake",
        ] {
            assert!(names.contains(&n.to_string()), "{n} in {names:?}");
            let cfg = PipelineConfig {
                embedder: StageCfg::named(n),
                ..PipelineConfig::default()
            };
            // Known whether or not this build runs it: the service embeds with its own
            // config; the pipeline only names the vectors it's measured with.
            Pipeline::build(&cfg, &reg).unwrap();
        }
        let cfg = PipelineConfig {
            embedder: StageCfg::named("word2vec"),
            ..PipelineConfig::default()
        };
        let err = Pipeline::build(&cfg, &reg).err().unwrap().to_string();
        assert!(err.contains("embedder \"word2vec\""), "{err}");
        assert!(err.contains("onnx-q8"), "{err}");
    }

    #[tokio::test]
    async fn an_unknown_stage_is_an_error_that_names_it() {
        let reg = registry().await;
        let cfg = PipelineConfig {
            retriever: StageCfg::named("telepathy"),
            ..PipelineConfig::default()
        };
        let err = Pipeline::build(&cfg, &reg).err().unwrap().to_string();
        assert!(err.contains("retriever \"telepathy\""), "{err}");
        assert!(err.contains("hybrid-rrf"), "{err}");
        // A name registered for another kind doesn't count.
        let cfg = PipelineConfig {
            packer: StageCfg::named("none"),
            ..PipelineConfig::default()
        };
        assert!(Pipeline::build(&cfg, &reg).is_err());
    }

    #[tokio::test]
    async fn fakes_register_under_the_names_they_fake() {
        struct Fake;
        impl Fusion for Fake {
            fn name(&self) -> &'static str {
                "fake"
            }
            fn fuse(&self, _legs: Vec<Vec<Hit>>, _cfg: &FusionCfg) -> Vec<Hit> {
                Vec::new()
            }
        }
        let mut reg = registry().await;
        reg.register(
            "rrf",
            StageCtor::Fusion(Arc::new(|_, _| Ok(Arc::new(Fake)))),
        );
        let p = Pipeline::build(&PipelineConfig::default(), &reg).unwrap();
        assert_eq!(p.fusion.name(), "fake");
    }

    fn hit(text: &str, ord: i64, score: f32) -> Hit {
        Hit {
            passage: PassageRow {
                id: format!("s1:{ord}"),
                project_id: String::new(),
                document_id: "s1".into(),
                origin: "source".into(),
                scope_ids: Vec::new(),
                access: vec![margin_shared::context::ACCESS_COMPANY.to_string()],
                unit_id: format!("s1:page:{ord}"),
                modality: "text".into(),
                ord,
                page: Some(1),
                start_line: None,
                end_line: None,
                start_ms: None,
                end_ms: None,
                section_path: String::new(),
                text: text.into(),
                file: None,
                chunk_hash: String::new(),
                embedder: String::new(),
                vector: Vec::new(),
            },
            score,
            dense_rank: None,
            lex_rank: None,
            rerank: None,
            highlight: None,
        }
    }

    #[test]
    fn a_citation_names_its_section() {
        let mut h = hit("alpha", 0, 0.9);
        assert_eq!(citation_section_path(&h), None);
        h.passage.section_path = " Chapter 2 > Fourier series ".into();
        assert_eq!(
            citation_section_path(&h).as_deref(),
            Some("Chapter 2 > Fourier series")
        );
        assert_eq!(
            hit_citation(&h).section_path.as_deref(),
            Some("Chapter 2 > Fourier series")
        );
    }

    #[tokio::test]
    async fn the_trivial_stages_work() {
        let reg = registry().await;
        let p = Pipeline::build(&trivial(), &reg).unwrap();
        let hits = vec![hit("alpha", 0, 0.9), hit("beta", 1, 0.5)];
        let hits = p.reranker.rerank("alpha", hits).await.unwrap();
        let fused = p.fusion.fuse(
            vec![hits],
            &FusionCfg {
                k_fused: 1,
                rrf_k: 60,
                params: Default::default(),
            },
        );
        assert_eq!(fused.len(), 1);
        assert_eq!(fused[0].passage.text, "alpha");
        let packed = p.packer.pack(
            fused,
            &PackBudget {
                tokens: 100,
                images_max: 0,
                min_score_pct: 0,
                passages_min: 0,
                k: 4,
                diversity: Diversity::default(),
            },
        );
        assert_eq!(packed[0].citation.page, Some(1));
        assert_eq!(packed[0].citation.document_id.as_deref(), Some("s1"));
        let chunks = p.chunker.chunk(
            &Extracted {
                pages: vec![margin_embed::Page {
                    number: None,
                    text: "One. Two.".into(),
                }],
                ..Default::default()
            },
            SourceKind::Text,
            &|t| t.split_whitespace().count(),
            &ChunkerCfg {
                max_tokens: 100,
                params: Default::default(),
            },
        );
        assert_eq!(chunks.len(), 1);
    }

    #[test]
    fn lesson_v2_lays_out_margin_ais_prompts_constant_first() {
        let ctx = LessonContext {
            subject_title: "Signals".into(),
            topic_title: "Fourier".into(),
            prompt: "Teach me Fourier series".into(),
            level: margin_shared::Level::Intro,
            with_quiz: false,
            outline: vec![margin_shared::OutlineLesson {
                title: "Series".into(),
                summary: "Sums of sines".into(),
                minutes: 10,
            }],
            index: 0,
            passages: Vec::new(),
        };
        let a = LessonV2.lesson(&ctx);
        assert_eq!(a.version, margin_ai::prompts::LESSON_VERSION);
        assert_eq!(a.constant, margin_ai::prompts::LESSON_CONSTANT);
        assert!(
            a.variable.contains("Teach me Fourier series"),
            "{}",
            a.variable
        );
        assert_eq!(a.cache_key, "lesson:lesson-v2");
        let other = LessonContext {
            prompt: "Something else".into(),
            ..ctx
        };
        assert_eq!(LessonV2.lesson(&other).constant, a.constant);
        let plan = LessonV2.plan(&PlanInput {
            request: "Learn DSP".into(),
            contents: vec!["dsp.md".into()],
            survey: Vec::new(),
        });
        assert_eq!(plan.constant, margin_ai::prompts::PLAN_CONSTANT);
        assert!(plan.variable.contains("dsp.md"));
        let rerank = LessonV2.rerank("gibbs", &[hit("ringing", 0, 1.0)]);
        assert!(rerank.variable.contains("s1:0"), "{}", rerank.variable);
        assert_eq!(LessonV2.version(), "lesson-v2");
    }
}
