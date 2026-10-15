//! The retrieval and generation pipeline as configuration (`docs/agent-rag-architecture.md`
//! §2.0). One `PipelineConfig` is read by the service (`Settings.pipeline`), the eval
//! runner (`evals/variants/*.toml`) and the tests; each stage is named and looked up in
//! the service's registry, so switching a variant is a settings change, not a release.
//!
//! Frozen in R1's first commit: the top-level keys of `PipelineConfig`, `StageCfg`,
//! `Budget`, `Route` and `RouteTable`. Stage names and their `params` move freely.

use serde::{Deserialize, Serialize};

/// Every stage by name plus its parameters, the prompt set's version, the model routes
/// and the context budget. Stamped on proposals, lessons, traces and eval runs as
/// `hash()`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineConfig {
    /// The embedding backend (`docs/plan-linux-service.md` §2.11): `"onnx-q8"`,
    /// `"onnx-fp32"`, `"onnx-fp16"`, or the test stand-ins `"recorded"` and `"fake"`; the
    /// default, `"coreml"` (`DEFAULT_EMBEDDER`), is the Mac service's backend, kept only
    /// as a name. Left out of the JSON (and so of `hash()`) while it is the default, so
    /// every hash stored before it existed keeps its meaning.
    #[serde(
        default = "default_embedder",
        skip_serializing_if = "is_default_embedder"
    )]
    pub embedder: StageCfg,
    /// Query rewriting before retrieval (R2, `docs/r2-brief.md` D3): `"none"` or
    /// `"small-expand-v1"`. Left out of the JSON (and so of `hash()`) while it is
    /// `"none"`, so every hash stored before it existed keeps its meaning.
    #[serde(
        default = "default_rewriter",
        skip_serializing_if = "is_default_rewriter"
    )]
    pub rewriter: StageCfg,
    #[serde(default = "default_chunker")]
    pub chunker: StageCfg,
    #[serde(default = "default_retriever")]
    pub retriever: StageCfg,
    #[serde(default = "default_fusion")]
    pub fusion: StageCfg,
    #[serde(default = "default_reranker")]
    pub reranker: StageCfg,
    #[serde(default = "default_packer")]
    pub packer: StageCfg,
    /// The prompt set's version, e.g. "lesson-v2".
    #[serde(default = "default_prompts")]
    pub prompts: String,
    #[serde(default)]
    pub routes: RouteTable,
    #[serde(default)]
    pub budget: Budget,
}

/// A stage: the name it is registered under and its parameters. Parameters a stage
/// doesn't know are kept (and hashed), so a config written by a newer build survives a
/// round trip through an older one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StageCfg {
    pub name: String,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub params: serde_json::Map<String, serde_json::Value>,
}

impl StageCfg {
    pub fn named(name: &str) -> Self {
        Self {
            name: name.to_string(),
            params: serde_json::Map::new(),
        }
    }
}

/// How much retrieved material a generation job may take. No fixed token cap (owner
/// Q3): the budget is a share of the route's model window, resolved per call.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Budget {
    /// Share of the model's context window passages may fill (0.5 = half).
    pub share_of_window: f32,
    /// Hits under this percentile of the top list's fused score are dropped.
    pub min_score_pct: u8,
    /// Most pictures sent with one call.
    pub images_max: u32,
    /// Fewest passages to keep even when the score threshold would drop more.
    pub passages_min: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Self {
            share_of_window: 0.5,
            min_score_pct: 25,
            images_max: 8,
            passages_min: 0,
        }
    }
}

/// Which model slot a call uses (§2.6): chats, writing, or small tasks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Route {
    /// Agent steps.
    Chat,
    /// Lessons, scenes, the outline planner.
    Write,
    /// Titles, condense, query rewrite, listwise rerank, eval questions.
    Small,
}

impl Route {
    pub const ALL: [Route; 3] = [Route::Chat, Route::Write, Route::Small];

    pub fn as_str(self) -> &'static str {
        match self {
            Route::Chat => "chat",
            Route::Write => "write",
            Route::Small => "small",
        }
    }
}

/// The model id for each route; `None` uses the provider's chat model.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RouteTable {
    pub chat: Option<String>,
    pub write: Option<String>,
    pub small: Option<String>,
}

impl RouteTable {
    pub fn get(&self, route: Route) -> Option<&str> {
        match route {
            Route::Chat => self.chat.as_deref(),
            Route::Write => self.write.as_deref(),
            Route::Small => self.small.as_deref(),
        }
    }
}

fn default_embedder() -> StageCfg {
    StageCfg::named(DEFAULT_EMBEDDER)
}
fn is_default_embedder(cfg: &StageCfg) -> bool {
    *cfg == default_embedder()
}
/// The `embedder` stage a config without one names: the backend every pipeline ran on
/// before the stage existed (Core ML, on the Mac service). No service runs it any more
/// (the service stamps its own ONNX backend); the name stays so stored hashes keep their
/// meaning.
pub const DEFAULT_EMBEDDER: &str = "coreml";
fn default_rewriter() -> StageCfg {
    StageCfg::named("none")
}
fn is_default_rewriter(cfg: &StageCfg) -> bool {
    *cfg == default_rewriter()
}
fn default_chunker() -> StageCfg {
    StageCfg::named("sections-v2")
}
fn default_retriever() -> StageCfg {
    StageCfg::named("hybrid-rrf")
}
fn default_fusion() -> StageCfg {
    StageCfg::named("rrf")
}
fn default_reranker() -> StageCfg {
    StageCfg::named("none")
}
fn default_packer() -> StageCfg {
    StageCfg::named("mmr-v1")
}
fn default_prompts() -> String {
    "lesson-v2".into()
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            embedder: default_embedder(),
            rewriter: default_rewriter(),
            chunker: default_chunker(),
            retriever: default_retriever(),
            fusion: default_fusion(),
            reranker: default_reranker(),
            packer: default_packer(),
            prompts: default_prompts(),
            routes: RouteTable::default(),
            budget: Budget::default(),
        }
    }
}

impl PipelineConfig {
    /// A stable 16-hex-digit fingerprint of the whole config: FNV-1a 64 over its JSON
    /// with object keys sorted, so it doesn't depend on map order or on the build.
    pub fn hash(&self) -> String {
        let value = serde_json::to_value(self).expect("a PipelineConfig is always JSON");
        let mut canonical = String::new();
        write_canonical(&value, &mut canonical);
        let h = canonical.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
            (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3)
        });
        format!("{h:016x}")
    }
}

/// The kinds of stage a pipeline is made of, in the order Settings lists them. Each but
/// "prompts" is a `StageCfg` of `PipelineConfig`; "prompts" is its prompt set's version.
/// "embedder" is the service's own (`service.toml`'s `embedder`): the service sets it on
/// every pipeline it runs, Settings shows it without a picker, and `with_stages_of` keeps
/// it; the eval runner loads the one a variant names.
pub const STAGE_KINDS: [&str; 8] = [
    "chunker",
    "embedder",
    "rewriter",
    "retriever",
    "fusion",
    "reranker",
    "packer",
    "prompts",
];

impl PipelineConfig {
    /// The name of the stage of `kind` (one of `STAGE_KINDS`); `None` for another kind.
    pub fn stage_name(&self, kind: &str) -> Option<&str> {
        Some(match kind {
            "chunker" => &self.chunker.name,
            "embedder" => &self.embedder.name,
            "rewriter" => &self.rewriter.name,
            "retriever" => &self.retriever.name,
            "fusion" => &self.fusion.name,
            "reranker" => &self.reranker.name,
            "packer" => &self.packer.name,
            "prompts" => &self.prompts,
            _ => return None,
        })
    }

    /// This config with the stage of `kind` named `name`. A stage that changes name
    /// loses its params (they were the old stage's); the same name keeps them. Another
    /// kind changes nothing.
    pub fn with_stage(&self, kind: &str, name: &str) -> PipelineConfig {
        let mut cfg = self.clone();
        let stage = match kind {
            "chunker" => &mut cfg.chunker,
            "embedder" => &mut cfg.embedder,
            "rewriter" => &mut cfg.rewriter,
            "retriever" => &mut cfg.retriever,
            "fusion" => &mut cfg.fusion,
            "reranker" => &mut cfg.reranker,
            "packer" => &mut cfg.packer,
            "prompts" => {
                cfg.prompts = name.to_string();
                return cfg;
            }
            _ => return cfg,
        };
        if stage.name != name {
            *stage = StageCfg::named(name);
        }
        cfg
    }

    /// This config's stages and prompts from `variant`; its model routes and budget stay
    /// (Models and the budget form set those), and so does its embedder (the service's).
    pub fn with_stages_of(&self, variant: &PipelineConfig) -> PipelineConfig {
        PipelineConfig {
            routes: self.routes.clone(),
            budget: self.budget,
            embedder: self.embedder.clone(),
            ..variant.clone()
        }
    }

    /// This config with `embedder` as its embedder stage (its params kept when the name
    /// is the same): what a service running that backend stores and stamps.
    pub fn with_embedder(&self, embedder: &str) -> PipelineConfig {
        self.with_stage("embedder", embedder)
    }

    /// Whether the stages and prompts are `variant`'s (routes, budget and embedder aside).
    pub fn has_stages_of(&self, variant: &PipelineConfig) -> bool {
        self.with_stages_of(variant) == *self
    }
}

/// Why a retrieval runs (R2, `docs/r2-brief.md` D1): stages may behave differently per
/// purpose (a listwise rerank is worth a model call for a lesson, not for every agent
/// search).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RetrievalPurpose {
    /// Retrieval for a lesson (`ask::write_lesson`).
    Lesson,
    /// Retrieval for the outline planner.
    Plan,
    /// The agent's `search` tool.
    Agent,
    /// The context API (`POST /api/context/search`, MCP).
    Context,
}

impl RetrievalPurpose {
    pub const ALL: [RetrievalPurpose; 4] = [
        RetrievalPurpose::Lesson,
        RetrievalPurpose::Plan,
        RetrievalPurpose::Agent,
        RetrievalPurpose::Context,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            RetrievalPurpose::Lesson => "lesson",
            RetrievalPurpose::Plan => "plan",
            RetrievalPurpose::Agent => "agent",
            RetrievalPurpose::Context => "context",
        }
    }
}

/// `GET routes::PIPELINE_VARIANTS`: what Settings can choose a pipeline from.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct PipelineVariants {
    /// The registered stage names per kind (`STAGE_KINDS`), sorted.
    pub stages: std::collections::BTreeMap<String, Vec<String>>,
    /// The named variants of `evals/variants/*.toml` that could be read, by name.
    pub variants: Vec<NamedVariant>,
}

/// One `evals/variants/<name>.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NamedVariant {
    pub name: String,
    pub config: PipelineConfig,
    /// Why this service can't build it (a stage it doesn't register); `None` when it can.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unavailable: Option<String>,
}

fn write_canonical(value: &serde_json::Value, out: &mut String) {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::Value::String(k.clone()).to_string());
                out.push(':');
                write_canonical(&map[k], out);
            }
            out.push('}');
        }
        serde_json::Value::Array(items) => {
            out.push('[');
            for (i, v) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(v, out);
            }
            out.push(']');
        }
        other => out.push_str(&other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn round_trips_and_fills_defaults() {
        let cfg = PipelineConfig::default();
        let json = serde_json::to_value(&cfg).unwrap();
        assert_eq!(
            serde_json::from_value::<PipelineConfig>(json.clone()).unwrap(),
            cfg
        );
        assert_eq!(json["chunker"], json!({"name": "sections-v2"}));
        assert_eq!(json["retriever"]["name"], "hybrid-rrf");
        assert_eq!(json["budget"]["share_of_window"], 0.5);
        // An empty table is the default.
        assert_eq!(serde_json::from_str::<PipelineConfig>("{}").unwrap(), cfg);
        let partial: PipelineConfig = serde_json::from_value(
            json!({"retriever": {"name": "dense"}, "budget": {"images_max": 2}}),
        )
        .unwrap();
        assert_eq!(partial.retriever.name, "dense");
        assert_eq!(partial.budget.images_max, 2);
        assert_eq!(partial.budget.min_score_pct, 25);
        assert_eq!(partial.packer.name, "mmr-v1");
    }

    #[test]
    fn default_hash_is_stable() {
        let a = PipelineConfig::default().hash();
        assert_eq!(a.len(), 16);
        // Pinned: a change here means every stored pipeline_hash changes meaning.
        assert_eq!(a, "c39a244aa537f0c2");
        let mut b = PipelineConfig::default();
        b.budget.images_max = 4;
        assert_ne!(a, b.hash());
        let mut c = PipelineConfig::default();
        c.routes.small = Some("gpt-6-luna".into());
        assert_ne!(a, c.hash());
    }

    #[test]
    fn the_embedder_stage_defaults_to_core_ml_and_hashes_only_when_changed() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.embedder.name, "coreml");
        assert!(
            serde_json::to_value(&cfg)
                .unwrap()
                .get("embedder")
                .is_none()
        );
        let onnx: PipelineConfig =
            serde_json::from_value(json!({"embedder": {"name": "onnx-q8"}})).unwrap();
        assert_eq!(onnx.embedder.name, "onnx-q8");
        assert_ne!(onnx.hash(), cfg.hash());
        let back: PipelineConfig =
            serde_json::from_value(serde_json::to_value(&onnx).unwrap()).unwrap();
        assert_eq!(back, onnx);
        // Naming the default explicitly is the default.
        let explicit: PipelineConfig =
            serde_json::from_value(json!({"embedder": {"name": "coreml"}})).unwrap();
        assert_eq!(explicit.hash(), cfg.hash());
    }

    #[test]
    fn the_rewriter_stage_defaults_to_none_and_hashes_only_when_changed() {
        let cfg = PipelineConfig::default();
        assert_eq!(cfg.rewriter.name, "none");
        assert!(
            serde_json::to_value(&cfg)
                .unwrap()
                .get("rewriter")
                .is_none()
        );
        // A config without the key, or naming "none", hashes as before R2.
        let absent: PipelineConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(absent.hash(), "c39a244aa537f0c2");
        let none: PipelineConfig =
            serde_json::from_value(json!({"rewriter": {"name": "none"}})).unwrap();
        assert_eq!(none.hash(), "c39a244aa537f0c2");
        let expand: PipelineConfig =
            serde_json::from_value(json!({"rewriter": {"name": "small-expand-v1"}})).unwrap();
        assert_eq!(expand.rewriter.name, "small-expand-v1");
        assert_ne!(expand.hash(), cfg.hash());
        let back: PipelineConfig =
            serde_json::from_value(serde_json::to_value(&expand).unwrap()).unwrap();
        assert_eq!(back, expand);
        assert_eq!(
            cfg.with_stage("rewriter", "small-expand-v1").rewriter.name,
            "small-expand-v1"
        );
    }

    #[test]
    fn retrieval_purposes() {
        assert_eq!(
            serde_json::to_value(RetrievalPurpose::Context).unwrap(),
            json!("context")
        );
        for p in RetrievalPurpose::ALL {
            assert_eq!(serde_json::to_value(p).unwrap(), json!(p.as_str()));
        }
    }

    #[test]
    fn unknown_params_are_kept_and_hashed() {
        let cfg: PipelineConfig = serde_json::from_value(json!({
            "packer": {"name": "mmr-v1", "params": {"lambda": 0.7, "future_knob": {"x": [1, 2]}}}
        }))
        .unwrap();
        assert_eq!(cfg.packer.params["future_knob"], json!({"x": [1, 2]}));
        let back: PipelineConfig =
            serde_json::from_value(serde_json::to_value(&cfg).unwrap()).unwrap();
        assert_eq!(back, cfg);
        assert_ne!(cfg.hash(), PipelineConfig::default().hash());
        // Key order doesn't change the hash.
        let reordered: PipelineConfig = serde_json::from_value(json!({
            "packer": {"params": {"future_knob": {"x": [1, 2]}, "lambda": 0.7}, "name": "mmr-v1"}
        }))
        .unwrap();
        assert_eq!(reordered.hash(), cfg.hash());
    }

    #[test]
    fn stages_are_set_by_kind() {
        let mut cfg = PipelineConfig::default();
        cfg.retriever.params.insert("k".into(), json!(3));
        cfg.routes.chat = Some("m".into());
        for kind in STAGE_KINDS {
            assert!(cfg.stage_name(kind).is_some(), "{kind}");
        }
        assert_eq!(cfg.stage_name("nope"), None);
        // The same name keeps its params; another name drops them.
        assert_eq!(cfg.with_stage("retriever", "hybrid-rrf"), cfg);
        let dense = cfg.with_stage("retriever", "dense");
        assert_eq!(dense.retriever, StageCfg::named("dense"));
        assert_eq!(
            dense.with_stage("prompts", "lesson-v3").prompts,
            "lesson-v3"
        );
        assert_eq!(cfg.with_stage("nope", "x"), cfg);
        // A variant's stages, with this config's routes and budget.
        let mut variant = PipelineConfig::default().with_stage("fusion", "first");
        variant.budget.images_max = 1;
        let picked = cfg.with_stages_of(&variant);
        assert_eq!(picked.fusion.name, "first");
        assert_eq!(picked.routes, cfg.routes);
        assert_eq!(picked.budget, cfg.budget);
        assert!(picked.has_stages_of(&variant));
        assert!(!cfg.has_stages_of(&variant));
        // The embedder is the service's: a variant naming another one doesn't change it.
        let onnx = cfg.with_embedder("onnx-q8");
        assert_eq!(onnx.stage_name("embedder"), Some("onnx-q8"));
        let picked = onnx.with_stages_of(&variant.with_embedder("onnx-fp32"));
        assert_eq!(picked.embedder.name, "onnx-q8");
        assert!(picked.has_stages_of(&variant));
    }

    #[test]
    fn routes() {
        let table = RouteTable {
            small: Some("m".into()),
            ..Default::default()
        };
        assert_eq!(table.get(Route::Small), Some("m"));
        assert_eq!(table.get(Route::Chat), None);
        assert_eq!(serde_json::to_value(Route::Write).unwrap(), json!("write"));
        assert_eq!(Route::ALL.map(Route::as_str), ["chat", "write", "small"]);
    }
}
