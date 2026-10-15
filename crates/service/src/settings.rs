//! Settings (the API key goes to the secret store and never comes back) and tool status.

use axum::Json;
use axum::extract::State;
use margin_shared::pipeline::Route;
use margin_shared::{Event, Provider, RouteModels, Settings, SettingsPatch, ToolStatus};

use axum::http::StatusCode;

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::pipeline::Pipeline;
use crate::principal::RequireAdmin;
use crate::secrets::{ANTHROPIC_API_KEY, OPENAI_API_KEY};
use crate::state::{App, AppState, SETTINGS_ID};

pub async fn get_settings(State(state): State<AppState>) -> AppResult<Json<Settings>> {
    Ok(Json(state.settings().await?))
}

fn check_dimension(name: &str, v: u32) -> AppResult<()> {
    if !(16..=7680).contains(&v) || !v.is_multiple_of(2) {
        return Err(AppError::bad_request(format!(
            "The render {name} must be an even number from 16 to 7680."
        )));
    }
    Ok(())
}

/// `PATCH /api/settings`: organisation admins only (403 `admin_required`); everyone
/// reads them (`GET`, no secrets in them), so members can pick a configured provider.
pub async fn patch_settings(
    State(state): State<AppState>,
    _admin: RequireAdmin,
    ApiJson(patch): ApiJson<SettingsPatch>,
) -> AppResult<Json<Settings>> {
    {
        let _w = state.write_lock.lock().await;
        let mut row = state.settings_row().await?;
        if let Some(url) = &patch.openai_base_url {
            let url = url.trim().trim_end_matches('/');
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || reqwest::Url::parse(url).is_err()
            {
                return Err(AppError::bad_request(
                    "The base URL must be an http(s) URL, like https://api.openai.com/v1.",
                ));
            }
            row.openai_base_url = url.to_string();
        }
        // The legacy OpenAI fields first, then the slots (which win when both come).
        if let Some(model) = &patch.openai_model {
            row.set_openai_model(model.trim());
        }
        if let Some(model) = &patch.agent_model {
            row.set_agent_model(
                model
                    .as_ref()
                    .map(|m| m.trim().to_string())
                    .filter(|m| !m.is_empty()),
            );
        }
        for (provider, slots) in [
            (Provider::OpenAi, &patch.openai_models),
            (Provider::ChatGpt, &patch.chatgpt_models),
            (Provider::Claude, &patch.claude_models),
        ] {
            if let Some(slots) = slots {
                row.set_slots(provider, checked_slots(provider, slots)?);
            }
        }
        if let Some(provider) = patch.default_provider {
            row.default_provider = Some(provider);
        }
        if let Some(effort) = patch.reasoning_effort {
            row.reasoning_effort = effort;
        }
        if let Some(voice) = &patch.voice {
            // pocket-tts voices licensed for non-commercial use only are never saved.
            if let Some(why) = voice.as_deref().and_then(crate::video::voice_refused) {
                return Err(AppError::new(
                    StatusCode::BAD_REQUEST,
                    "voice_not_allowed",
                    why,
                ));
            }
            row.voice = voice
                .as_ref()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
        }
        if let Some(w) = patch.render_width {
            check_dimension("width", w)?;
            row.render_width = w;
        }
        if let Some(h) = patch.render_height {
            check_dimension("height", h)?;
            row.render_height = h;
        }
        if let Some(fps) = patch.render_fps {
            if !(1..=120).contains(&fps) {
                return Err(AppError::bad_request(
                    "The frame rate must be from 1 to 120.",
                ));
            }
            row.render_fps = fps;
        }
        if let Some(days) = patch.trace_retention_days {
            if !(1..=3650).contains(&days) {
                return Err(AppError::bad_request(
                    "Traces can be kept from 1 to 3650 days.",
                ));
            }
            row.trace_retention_days = days;
        }
        let rebuilt = match &patch.pipeline {
            Some(cfg) => {
                // The embedder is the one this service runs, whatever the patch names.
                let cfg = cfg.with_embedder(state.model_files.backend.name());
                let pipeline = Pipeline::build(&cfg, &state.registry).map_err(|e| {
                    AppError::new(StatusCode::BAD_REQUEST, "bad_pipeline", format!("{e:#}."))
                })?;
                row.pipeline = cfg;
                Some(pipeline)
            }
            None => None,
        };
        match patch.openai_api_key.as_deref().map(str::trim) {
            Some("") => state.secret_delete(OPENAI_API_KEY).await?,
            Some(key) => state.secret_set(OPENAI_API_KEY, key.to_string()).await?,
            None => {}
        }
        match patch.anthropic_api_key.as_deref().map(str::trim) {
            Some("") => state.secret_delete(ANTHROPIC_API_KEY).await?,
            Some(key) => state.secret_set(ANTHROPIC_API_KEY, key.to_string()).await?,
            None => {}
        }
        state.store.put(db::SETTINGS, SETTINGS_ID, &row).await?;
        if let Some(pipeline) = rebuilt {
            state.pipeline.store(std::sync::Arc::new(pipeline));
        }
    }
    // A key, a base URL or a slot may have changed: the model lists are fetched again.
    state.providers.forget_models();
    let settings = state.settings().await?;
    state.emit(Event::SettingsChanged {
        settings: settings.clone(),
    });
    Ok(Json(settings))
}

/// Sends `SettingsChanged` with the settings as they are now (a ChatGPT sign-in or
/// sign-out, a refresh that signed out). Logged when they can't be read.
pub async fn emit_settings(state: &App) {
    match state.settings().await {
        Ok(settings) => state.emit(Event::SettingsChanged { settings }),
        Err(e) => tracing::error!("couldn't read the settings to announce them: {e:#}"),
    }
}

/// Slots from a patch: trimmed, none empty.
fn checked_slots(provider: Provider, slots: &RouteModels) -> AppResult<RouteModels> {
    let mut out = RouteModels::default();
    for route in Route::ALL {
        let id = slots.get(route).trim();
        if id.is_empty() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "no_model",
                format!(
                    "Every {} slot needs a model: chats, writing and small tasks.",
                    provider.label()
                ),
            ));
        }
        out.set(route, id);
    }
    Ok(out)
}

pub async fn tools(State(state): State<AppState>) -> Json<Vec<ToolStatus>> {
    Json(state.renderer.detect_tools().await)
}

/// The stage kinds a `PipelineConfig` names, in the order the pipeline runs them.
pub use margin_shared::pipeline::{NamedVariant, PipelineVariants, STAGE_KINDS};

/// The named variants built in (`evals/variants/*.toml`), so a deployed service, which
/// has no checkout next to it, still offers them.
pub const BUILTIN_VARIANTS: &[(&str, &str)] = &[
    (
        "baseline",
        include_str!("../../../evals/variants/baseline.toml"),
    ),
    ("bm25", include_str!("../../../evals/variants/bm25.toml")),
    ("dense", include_str!("../../../evals/variants/dense.toml")),
    (
        "hybrid",
        include_str!("../../../evals/variants/hybrid.toml"),
    ),
    // R2 (`docs/r2-brief.md` D6): listed everywhere, unavailable where the service has
    // no small-tasks model or no reranker download.
    (
        "r2-full",
        include_str!("../../../evals/variants/r2-full.toml"),
    ),
    (
        "rerank-small",
        include_str!("../../../evals/variants/rerank-small.toml"),
    ),
    (
        "rerank-xenc",
        include_str!("../../../evals/variants/rerank-xenc.toml"),
    ),
    (
        "rerank-xenc-bge",
        include_str!("../../../evals/variants/rerank-xenc-bge.toml"),
    ),
    (
        "rewrite-small",
        include_str!("../../../evals/variants/rewrite-small.toml"),
    ),
];

/// The built-in variants that need a model-backed stage (R2): unavailable in a registry
/// without `StageDeps.models` or the reranker download.
pub const MODEL_BACKED_VARIANTS: &[&str] = &[
    "r2-full",
    "rerank-small",
    "rerank-xenc",
    "rerank-xenc-bge",
    "rewrite-small",
];

/// A folder of more variants: `MARGIN_EVALS_DIR`, when set (its `variants/*.toml` add to
/// the built-in ones and replace those of the same name).
fn evals_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("MARGIN_EVALS_DIR").map(Into::into)
}

/// One variant from its TOML; `None` (logged) when it doesn't parse.
fn parse_variant(
    name: String,
    text: &str,
    from: &str,
    registry: &crate::pipeline::Registry,
) -> Option<NamedVariant> {
    match toml::from_str::<margin_shared::pipeline::PipelineConfig>(text) {
        Ok(config) => {
            let unavailable = Pipeline::build(&config, registry)
                .err()
                .map(|e| format!("{e:#}"));
            Some(NamedVariant {
                name,
                config,
                unavailable,
            })
        }
        Err(e) => {
            tracing::warn!("couldn't read the variant {from}: {e:#}");
            None
        }
    }
}

/// The built-in variants and those in `dir/variants` (which replace a built-in one of
/// the same name), by name. A file that can't be read or parsed is skipped (logged); a
/// missing folder adds nothing.
pub fn named_variants(
    dir: Option<&std::path::Path>,
    registry: &crate::pipeline::Registry,
) -> Vec<NamedVariant> {
    let mut by_name: std::collections::BTreeMap<String, NamedVariant> = BUILTIN_VARIANTS
        .iter()
        .filter_map(|(name, text)| {
            parse_variant(
                name.to_string(),
                text,
                &format!("{name} (built in)"),
                registry,
            )
        })
        .map(|v| (v.name.clone(), v))
        .collect();
    let entries = dir.and_then(|d| std::fs::read_dir(d.join("variants")).ok());
    for p in entries
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|x| x == "toml"))
    {
        let Some(name) = p.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
            continue;
        };
        let text = match std::fs::read_to_string(&p) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("couldn't read the variant {}: {e}", p.display());
                continue;
            }
        };
        if let Some(v) = parse_variant(name, &text, &p.display().to_string(), registry) {
            by_name.insert(v.name.clone(), v);
        }
    }
    by_name.into_values().collect()
}

/// `GET /api/pipeline/variants`.
pub async fn pipeline_variants(State(state): State<AppState>) -> Json<PipelineVariants> {
    // Only the embedder this service runs: it is set in `service.toml`, not in Settings.
    let stages = STAGE_KINDS
        .iter()
        .map(|&k| {
            let names = if k == "embedder" {
                vec![state.model_files.backend.name().to_string()]
            } else {
                state.registry.names(k)
            };
            (k.to_string(), names)
        })
        .collect();
    let dir = evals_dir();
    let variants = {
        let state = state.clone();
        tokio::task::spawn_blocking(move || named_variants(dir.as_deref(), &state.registry))
            .await
            .unwrap_or_default()
    };
    Json(PipelineVariants { stages, variants })
}

/// Every prompt the service sends (R2, `docs/r2-brief.md` D4.1): `margin_ai`'s catalog
/// plus the service's own (`agent` with `AGENT_PROMPT_VERSION`, `title` and `condense`
/// with their `trace::prompt_version` names), sorted by job.
pub fn prompts_catalog() -> Vec<margin_shared::context::PromptInfo> {
    let mut catalog = margin_ai::prompts::catalog();
    catalog.extend(service_prompts());
    catalog.sort_by(|a, b| a.job.cmp(&b.job));
    catalog
}

/// The prompts the service itself sends, beside `margin_ai`'s: the agent's system
/// prompt, the chat title (versioned by its hash, `trace::prompt_version`) and the
/// agent loop's condense call.
fn service_prompts() -> Vec<margin_shared::context::PromptInfo> {
    vec![
        margin_ai::prompts::prompt_info(
            "agent",
            margin_agent::AGENT_PROMPT_VERSION,
            margin_agent::system_prompt(),
        ),
        margin_ai::prompts::prompt_info(
            "title",
            &crate::trace::prompt_version("title", crate::turns::TITLE_INSTRUCTIONS),
            crate::turns::TITLE_INSTRUCTIONS,
        ),
        margin_ai::prompts::prompt_info(
            "condense",
            margin_agent::SUMMARY_PROMPT_VERSION,
            margin_agent::SUMMARY_INSTRUCTIONS,
        ),
    ]
}

/// `GET /api/pipeline/prompts` → `prompts_catalog()`.
pub async fn pipeline_prompts(
    State(_state): State<AppState>,
) -> Json<Vec<margin_shared::context::PromptInfo>> {
    Json(prompts_catalog())
}

#[cfg(test)]
mod variant_tests {
    use super::*;
    use crate::pipeline::{Registry, StageDeps};

    #[tokio::test]
    async fn the_repositorys_variants_are_listed_and_buildable() {
        let store = crate::db::Store::memory().await.unwrap();
        let reg = Registry::standard(StageDeps::new(store));
        let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals");
        let variants = named_variants(Some(&repo), &reg);
        let names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
        for n in ["baseline", "bm25", "dense", "hybrid"] {
            assert!(names.contains(&n), "{n} missing from {names:?}");
        }
        // R2's model-backed variants can't be built without small-tasks models (or the
        // reranker download); every other one can.
        for v in &variants {
            if MODEL_BACKED_VARIANTS.contains(&v.name.as_str()) {
                assert!(v.unavailable.is_some(), "{}", v.name);
            } else {
                assert_eq!(v.unavailable, None, "{}", v.name);
            }
        }
        // A deployed service has no evals folder: the built-in variants are still there.
        let all: Vec<&str> = BUILTIN_VARIANTS.iter().map(|(n, _)| *n).collect();
        for dir in [None, Some(std::path::Path::new("/nonexistent"))] {
            let built_in = named_variants(dir, &reg);
            let names: Vec<&str> = built_in.iter().map(|v| v.name.as_str()).collect();
            assert_eq!(names, all);
            assert!(
                built_in
                    .iter()
                    .filter(|v| !MODEL_BACKED_VARIANTS.contains(&v.name.as_str()))
                    .all(|v| v.unavailable.is_none())
            );
        }
        // With small-tasks models (and the reranker download), they build too.
        let mut deps = StageDeps::new(crate::db::Store::memory().await.unwrap());
        deps.models = Some(std::sync::Arc::new(
            crate::rerank::fakes::FakeModels::default(),
        ));
        let with_models = Registry::standard(deps);
        for v in named_variants(Some(&repo), &with_models) {
            assert_eq!(v.unavailable, None, "{}", v.name);
        }
        // Every variant file in the repository is built in.
        let files = std::fs::read_dir(repo.join("variants")).unwrap().count();
        assert_eq!(files, BUILTIN_VARIANTS.len());
        assert!(
            reg.names("retriever")
                .contains(&"hybrid-linear".to_string())
        );
        for (kind, name) in [
            ("reranker", "listwise-small"),
            ("reranker", "onnx-crossencoder"),
            ("rewriter", "small-expand-v1"),
        ] {
            assert!(reg.names(kind).contains(&name.to_string()), "{kind} {name}");
        }
    }

    /// The `[job]` tables of `evals/prompts.lock`: job → (version, constant_hash).
    fn lock() -> std::collections::BTreeMap<String, (String, String)> {
        let path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../evals/prompts.lock");
        let text = std::fs::read_to_string(path).unwrap();
        let table: toml::Table = toml::from_str(&text).unwrap();
        table
            .into_iter()
            .map(|(job, v)| {
                let s = |k: &str| v[k].as_str().unwrap().to_string();
                (job, (s("version"), s("constant_hash")))
            })
            .collect()
    }

    #[test]
    fn the_prompt_catalog_lists_every_prompt_once_and_matches_the_lock() {
        let catalog = prompts_catalog();
        let jobs: Vec<&str> = catalog.iter().map(|p| p.job.as_str()).collect();
        for job in ["agent", "lesson", "plan", "rerank", "rewrite", "scene"] {
            assert!(jobs.contains(&job), "{job} missing from {jobs:?}");
        }
        let mut sorted = jobs.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted, jobs, "sorted by job, each once");
        let agent = catalog.iter().find(|p| p.job == "agent").unwrap();
        assert_eq!(agent.version, margin_agent::AGENT_PROMPT_VERSION);
        // The same version means the same bytes: every locked job matches.
        let lock = lock();
        for p in &catalog {
            if let Some((version, hash)) = lock.get(&p.job)
                && *version == p.version
            {
                assert_eq!(*hash, p.constant_hash, "{} {}", p.job, p.version);
            }
        }
        assert!(lock.contains_key("agent"));
    }
}
