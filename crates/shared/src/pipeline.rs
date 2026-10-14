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
