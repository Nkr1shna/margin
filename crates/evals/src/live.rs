//! The real things the opt-in runner uses, and only the runner: the OpenAI Responses
//! and Claude Messages clients with the owner's keys from the service's secret store
//! (its secrets file), the real embedder (ONNX Runtime), and the readers
//! (pdfium/tesseract). Nothing here runs in tests or CI: [`guard`] refuses when `CI` (or
//! `MARGIN_EVAL_NO_LIVE`) is set, and every test drives these clients against local
//! fakes, never a provider.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use margin_ai::{ResponseRequest, ResponsesClient, StreamEvent};
use margin_embed::Backend;
use margin_service::engines::BoxFuture;
use margin_shared::context::TraceUsage;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::{EvalModel, ModelReply, ModelRequest};

/// The secrets the runner reads (the service's names).
pub const OPENAI_KEY: &str = margin_service::secrets::OPENAI_API_KEY;
pub const ANTHROPIC_KEY: &str = margin_service::secrets::ANTHROPIC_API_KEY;

/// Picks the embedding backend for a run: `MARGIN_EVAL_EMBEDDER` when set; else the
/// variant's `embedder` stage when the command runs one (`variant_embedder`); else the
/// service config's `embedder`. An unknown name or a stage that isn't a backend
/// (`recorded`, `fake`) is an error that says where it came from.
pub fn live_backend(
    env: &dyn Fn(&str) -> Option<String>,
    variant_embedder: Option<&str>,
    config_embedder: &str,
) -> anyhow::Result<Backend> {
    let (name, from) = match env("MARGIN_EVAL_EMBEDDER").filter(|v| !v.trim().is_empty()) {
        Some(v) => (v, "MARGIN_EVAL_EMBEDDER".to_string()),
        None => match variant_embedder {
            Some(v) => (v.to_string(), "the variant's [embedder]".to_string()),
            None => (
                config_embedder.to_string(),
                "the service config's embedder".to_string(),
            ),
        },
    };
    name.parse()
        .with_context(|| format!("{from} doesn't name an embedding backend"))
}

/// The service's config, or its defaults when there is none (a machine that only runs
/// evals).
pub fn service_config() -> anyhow::Result<margin_service::config::Config> {
    let path = margin_service::config::config_path();
    if path.exists() {
        margin_service::config::Config::load(&path)
    } else {
        Ok(margin_service::config::Config::default())
    }
}

/// The store the service keeps its keys in (`service.toml`'s `secrets`/`secrets_file`).
pub fn secret_store(
    config: &margin_service::config::Config,
) -> anyhow::Result<Arc<dyn margin_service::secrets::SecretStore>> {
    config.secret_store()
}

/// The readers `extract_corpus` uses, as the extraction records them.
pub const READER: &str = "pdfium+tesseract";

/// The readers `extract_corpus` uses (`READER`).
pub fn reader_name() -> &'static str {
    READER
}

/// Where the corpus as `reader` read it lives under `evals/`: the service's readers'
/// (`READER`) at `recorded/extracted.json` (`EXTRACTED_FILE`), any other's beside it
/// (`recorded/extracted-{reader}.json`): another OCR engine cuts other chunks.
pub fn extraction_file(reader: &str) -> PathBuf {
    if reader == READER {
        PathBuf::from(crate::corpus::EXTRACTED_FILE)
    } else {
        PathBuf::from(format!("recorded/extracted-{reader}.json"))
    }
}

/// The OS the run is on, for the extraction's record: the Linux distribution's name
/// (`Ubuntu 24.04.3 LTS`), else the OS's.
pub fn os_version() -> String {
    std::fs::read_to_string("/etc/os-release")
        .ok()
        .and_then(|t| os_release_name(&t))
        .unwrap_or_else(|| std::env::consts::OS.to_string())
}

/// `PRETTY_NAME` (else `NAME` and `VERSION_ID`) from `/etc/os-release`.
pub fn os_release_name(text: &str) -> Option<String> {
    let field = |key: &str| {
        text.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.strip_prefix('=')?;
            let v = v.trim().trim_matches('"').trim_matches('\'');
            (!v.is_empty()).then(|| v.to_string())
        })
    };
    field("PRETTY_NAME").or_else(|| {
        let name = field("NAME")?;
        Some(match field("VERSION_ID") {
            Some(v) => format!("{name} {v}"),
            None => name,
        })
    })
}

/// Where a backend's recorded vectors live under `evals/`: the default backend's (ONNX
/// 8-bit, the baseline) at `recorded/embeddings.bin` (`EMBEDDINGS_FILE`), every other
/// backend's beside it, named by its version
/// (`recorded/embeddings-embeddinggemma-2-onnx-fp32+title-prefix-v1.bin`), so recording
/// one never overwrites another.
pub fn recording_file(backend: Backend) -> PathBuf {
    if backend == Backend::default() {
        PathBuf::from(crate::recorded::EMBEDDINGS_FILE)
    } else {
        PathBuf::from(format!(
            "recorded/embeddings-{}.bin",
            backend.embedder_version().replace('/', "+")
        ))
    }
}

/// Refuses to reach a real model or the real embedder in CI or when told not to.
pub fn guard(env: &dyn Fn(&str) -> Option<String>) -> anyhow::Result<()> {
    for var in ["CI", "MARGIN_EVAL_NO_LIVE"] {
        if env(var).is_some_and(|v| !v.is_empty() && v != "0" && v != "false") {
            anyhow::bail!(
                "margin-eval won't call real models or load the real embedder with {var} set: \
                 CI runs `cargo test -p margin-evals` (recorded fixtures) instead"
            );
        }
    }
    Ok(())
}

pub fn guard_env() -> anyhow::Result<()> {
    guard(&|k| std::env::var(k).ok())
}

/// `evals/runner.toml`: endpoints and the model for each slot and provider.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RunnerConfig {
    pub openai_base_url: String,
    pub anthropic_base_url: String,
    pub models: RunnerModels,
    /// The context window the lesson budget is a share of, for the writing model.
    pub window_tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct RunnerModels {
    pub openai_chat: String,
    pub openai_write: String,
    pub openai_small: String,
    pub anthropic_write: String,
    pub anthropic_small: String,
}

impl RunnerConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        toml::from_str(
            &std::fs::read_to_string(path)
                .with_context(|| format!("couldn't read {}", path.display()))?,
        )
        .with_context(|| format!("bad {}", path.display()))
    }
}

/// An OpenAI-compatible Responses API model.
pub struct OpenAiModel {
    pub client: ResponsesClient,
    pub model: String,
}

impl OpenAiModel {
    pub fn new(base_url: &str, key: &str, model: &str) -> Self {
        Self {
            client: ResponsesClient::new(base_url, key, model),
            model: model.into(),
        }
    }
}

fn user_content(req: &ModelRequest) -> Value {
    let mut content = vec![json!({"type": "input_text", "text": req.user})];
    for i in &req.images {
        content.push(json!({"type": "input_image", "image_url": i.data_url(), "detail": "auto"}));
    }
    json!([{"role": "user", "content": content}])
}

impl EvalModel for OpenAiModel {
    fn id(&self) -> String {
        format!("openai:{}", self.model)
    }
    fn provider(&self) -> &str {
        "openai"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn complete<'a>(&'a self, req: &'a ModelRequest) -> BoxFuture<'a, anyhow::Result<ModelReply>> {
        Box::pin(async move {
            let r = ResponseRequest {
                model: Some(self.model.clone()),
                instructions: Some(req.system.clone()),
                input: user_content(req).as_array().cloned().unwrap_or_default(),
                tools: Vec::new(),
                tool_choice: None,
                reasoning: None,
                include_encrypted_reasoning: false,
                max_output_tokens: Some(req.max_output_tokens),
                prompt_cache_key: req.cache_key.clone(),
                context_management: None,
                parallel_tool_calls: None,
            };
            let mut stream = self
                .client
                .stream_response(&r)
                .await
                .with_context(|| format!("calling {}", self.id()))?;
            let mut text = String::new();
            while let Some(ev) = stream.next().await {
                match ev? {
                    StreamEvent::OutputTextDelta { delta, .. } => text.push_str(&delta),
                    StreamEvent::Completed { usage, .. } => {
                        return Ok(ModelReply {
                            text,
                            usage: TraceUsage {
                                input: usage.input_tokens,
                                cached: usage.cached_input_tokens,
                                output: usage.output_tokens,
                                reasoning: usage.reasoning_tokens,
                                ..Default::default()
                            },
                        });
                    }
                    StreamEvent::Incomplete { reason, .. } => {
                        anyhow::bail!("{} stopped early: {reason}", self.id())
                    }
                    StreamEvent::Failed { message, .. } | StreamEvent::Error { message, .. } => {
                        anyhow::bail!("{} failed: {message}", self.id())
                    }
                    _ => {}
                }
            }
            anyhow::bail!("{}'s answer ended before it finished", self.id())
        })
    }
}

/// A Claude model through the Messages API.
pub struct AnthropicModel {
    pub http: reqwest::Client,
    pub base_url: String,
    pub key: String,
    pub model: String,
}

impl AnthropicModel {
    pub fn new(base_url: &str, key: &str, model: &str) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').into(),
            key: key.into(),
            model: model.into(),
        }
    }

    pub fn body(&self, req: &ModelRequest) -> Value {
        let mut content = Vec::new();
        for i in &req.images {
            use base64::Engine;
            content.push(json!({"type": "image", "source": {
                "type": "base64",
                "media_type": i.media_type,
                "data": base64::engine::general_purpose::STANDARD.encode(&i.data),
            }}));
        }
        content.push(json!({"type": "text", "text": req.user}));
        let mut system = req.system.clone();
        if let Some((_, schema)) = &req.format {
            system.push_str(&format!(
                "\n\nAnswer with one JSON object matching this JSON schema exactly:\n{schema}"
            ));
        }
        json!({
            "model": self.model,
            "max_tokens": req.max_output_tokens,
            "system": [{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
            "messages": [{"role": "user", "content": content}],
        })
    }
}

impl EvalModel for AnthropicModel {
    fn id(&self) -> String {
        format!("anthropic:{}", self.model)
    }
    fn provider(&self) -> &str {
        "anthropic"
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn complete<'a>(&'a self, req: &'a ModelRequest) -> BoxFuture<'a, anyhow::Result<ModelReply>> {
        Box::pin(async move {
            let url = format!("{}/v1/messages", self.base_url);
            let resp = self
                .http
                .post(&url)
                .header("x-api-key", &self.key)
                .header("anthropic-version", "2023-06-01")
                .json(&self.body(req))
                .send()
                .await
                .with_context(|| format!("calling {}", self.id()))?;
            let status = resp.status();
            let v: Value = resp
                .json()
                .await
                .with_context(|| format!("reading {}'s answer", self.id()))?;
            if !status.is_success() {
                anyhow::bail!(
                    "{} answered {status}: {}",
                    self.id(),
                    v.pointer("/error/message")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                );
            }
            let text = v["content"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|c| c["type"] == "text")
                .filter_map(|c| c["text"].as_str())
                .collect::<Vec<_>>()
                .join("");
            let n = |k: &str| v["usage"][k].as_u64().unwrap_or(0);
            let cached = n("cache_read_input_tokens");
            let written = n("cache_creation_input_tokens");
            Ok(ModelReply {
                text,
                usage: TraceUsage {
                    // Claude's input_tokens leaves out cache reads and writes.
                    input: n("input_tokens") + cached + written,
                    cached,
                    cache_write: written,
                    output: n("output_tokens"),
                    ..Default::default()
                },
            })
        })
    }
}

/// The models a run uses, by slot, from the configured keys: OpenAI for chat; for
/// writing and small tasks the configured provider(s); a second judge and grader on
/// the other provider when both keys exist.
pub struct Models {
    pub writer: Arc<dyn EvalModel>,
    pub small: Vec<Arc<dyn EvalModel>>,
    pub chat: Option<(ResponsesClient, String)>,
}

pub fn models(
    cfg: &RunnerConfig,
    secrets: &dyn margin_service::secrets::SecretStore,
) -> anyhow::Result<Models> {
    let openai = secrets.get(OPENAI_KEY)?.filter(|k| !k.is_empty());
    let anthropic = secrets.get(ANTHROPIC_KEY)?.filter(|k| !k.is_empty());
    let m = &cfg.models;
    let mut small: Vec<Arc<dyn EvalModel>> = Vec::new();
    let mut writer: Option<Arc<dyn EvalModel>> = None;
    if let Some(k) = &anthropic {
        writer = Some(Arc::new(AnthropicModel::new(
            &cfg.anthropic_base_url,
            k,
            &m.anthropic_write,
        )));
        small.push(Arc::new(AnthropicModel::new(
            &cfg.anthropic_base_url,
            k,
            &m.anthropic_small,
        )));
    }
    if let Some(k) = &openai {
        if writer.is_none() {
            writer = Some(Arc::new(OpenAiModel::new(
                &cfg.openai_base_url,
                k,
                &m.openai_write,
            )));
        }
        small.push(Arc::new(OpenAiModel::new(
            &cfg.openai_base_url,
            k,
            &m.openai_small,
        )));
    }
    let writer = writer.context(
        "no model API key in the service's secret store (openai-api-key or anthropic-api-key); set one in Margin's Settings",
    )?;
    Ok(Models {
        writer,
        small,
        chat: openai.map(|k| {
            (
                ResponsesClient::new(&cfg.openai_base_url, k, &m.openai_chat),
                m.openai_chat.clone(),
            )
        }),
    })
}

/// The real embedder's vectors: EmbeddingGemma 2 on the chosen backend (`live_backend`),
/// pictures through its image encoder (a PDF page from the render `extract` wrote under
/// `media`).
pub struct LiveVectors {
    pub embedder: Arc<dyn margin_embed::Embedder>,
    pub backend: Backend,
    pub corpus: PathBuf,
    pub media: PathBuf,
}

/// Where a document's page renders go.
pub fn media_dir(media: &Path, document: &str) -> PathBuf {
    media.join(document.replace('/', "_"))
}

/// The folder `backend`'s model is in: `MARGIN_EMBED_MODEL_DIR` when set, else the
/// service's (`data_dir/models/<backend folder>`, where Settings and `margin-service
/// download-model` put it).
pub fn model_dir(
    env: &dyn Fn(&str) -> Option<String>,
    config: &margin_service::config::Config,
    backend: Backend,
) -> PathBuf {
    match env("MARGIN_EMBED_MODEL_DIR").filter(|d| !d.is_empty()) {
        Some(d) => PathBuf::from(d),
        None => config.data_dir.join("models").join(backend.dir_name()),
    }
}

impl LiveVectors {
    /// Loads `backend`'s model from `model_dir`.
    pub fn load(
        corpus: &Path,
        media: &Path,
        config: &margin_service::config::Config,
        backend: Backend,
    ) -> anyhow::Result<Self> {
        let dir = model_dir(&|k| std::env::var(k).ok(), config, backend);
        let files = margin_embed::ModelFiles::with_backend(&dir, backend);
        anyhow::ensure!(
            files.is_complete(),
            "the {} model isn't downloaded in {} (download it in Margin's Settings, or run \
             `margin-service download-model --embedder {backend}`)",
            backend.label(),
            dir.display()
        );
        Ok(Self {
            embedder: margin_embed::load_embedder(&files)?,
            backend,
            corpus: corpus.into(),
            media: media.into(),
        })
    }
}

impl crate::recorded::Vectors for LiveVectors {
    fn version(&self) -> &str {
        self.embedder.version()
    }
    fn texts(
        &self,
        texts: &[String],
        purpose: &margin_embed::Purpose,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        Ok(self.embedder.embed(texts, purpose.clone())?)
    }
    fn picture(
        &self,
        doc: &crate::corpus::CorpusDoc,
        pic: &margin_embed::Picture,
        _text: &str,
    ) -> anyhow::Result<Vec<f32>> {
        let path = match pic.page {
            Some(_) if doc.kind == margin_shared::SourceKind::Pdf => {
                media_dir(&self.media, &doc.path).join(&pic.path)
            }
            _ => self.corpus.join(&doc.path),
        };
        Ok(self.embedder.embed_images(&[path], pic.tokens)?.remove(0))
    }
}

/// Reads every PDF and image of the corpus with the real readers (pdfium and
/// tesseract), rendering PDF pages under `media`.
pub fn extract_corpus(
    corpus: &Path,
    docs: &[crate::corpus::CorpusDoc],
    media: &Path,
    reader: &str,
) -> anyhow::Result<crate::corpus::Extraction> {
    use crate::corpus::{ExtractedDoc, PageText, PictureRef};
    let mut out = crate::corpus::Extraction {
        reader: reader.into(),
        documents: Default::default(),
    };
    for doc in docs {
        if !matches!(
            doc.kind,
            margin_shared::SourceKind::Pdf | margin_shared::SourceKind::Image
        ) {
            continue;
        }
        let dir = media_dir(media, &doc.path);
        std::fs::create_dir_all(&dir)?;
        let ex = margin_embed::extract(&corpus.join(&doc.path), doc.kind, &dir)
            .with_context(|| format!("couldn't read {}", doc.path))?;
        out.documents.insert(
            doc.path.clone(),
            ExtractedDoc {
                pages: ex
                    .pages
                    .iter()
                    .map(|p| PageText {
                        number: p.number,
                        text: p.text.clone(),
                    })
                    .collect(),
                pictures: ex
                    .pictures
                    .iter()
                    .map(|p| PictureRef { page: p.page })
                    .collect(),
            },
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_guard_refuses_in_ci() {
        assert!(guard(&|_| None).is_ok());
        assert!(guard(&|k| (k == "CI").then(|| "true".into())).is_err());
        assert!(guard(&|k| (k == "CI").then(|| "0".into())).is_ok());
        let err = guard(&|k| (k == "MARGIN_EVAL_NO_LIVE").then(|| "1".into()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("cargo test -p margin-evals"), "{err}");
    }

    #[test]
    fn the_backend_comes_from_the_env_the_variant_or_the_config() {
        use margin_embed::OnnxDtype;
        let none = |_: &str| None;
        let here = Backend::default();
        // The config's, when nothing else says.
        assert_eq!(live_backend(&none, None, here.name()).unwrap(), here);
        // The variant's stage wins over the config.
        assert_eq!(
            live_backend(&none, Some("onnx-fp32"), "onnx-q8").unwrap(),
            Backend::Onnx(OnnxDtype::Fp32)
        );
        // The environment wins over both.
        let env = |k: &str| (k == "MARGIN_EVAL_EMBEDDER").then(|| here.name().to_string());
        assert_eq!(live_backend(&env, Some("tf"), "tf").unwrap(), here);
        // Not a backend (Core ML's is gone): said plainly.
        let err = live_backend(&none, Some("recorded"), "onnx-q8")
            .unwrap_err()
            .to_string();
        assert!(err.contains("variant's [embedder]"), "{err}");
        let err = live_backend(&none, None, "coreml").unwrap_err().to_string();
        assert!(err.contains("service config's embedder"), "{err}");
    }

    #[test]
    fn recordings_and_model_folders_are_per_backend() {
        use margin_embed::OnnxDtype;
        assert_eq!(
            recording_file(Backend::Onnx(OnnxDtype::Q8)),
            PathBuf::from(crate::recorded::EMBEDDINGS_FILE)
        );
        assert_eq!(
            recording_file(Backend::Onnx(OnnxDtype::Fp32)),
            PathBuf::from("recorded/embeddings-embeddinggemma-2-onnx-fp32+title-prefix-v1.bin")
        );
        let c = margin_service::config::Config {
            data_dir: PathBuf::from("/srv/data"),
            ..Default::default()
        };
        let none = |_: &str| None;
        assert_eq!(
            model_dir(&none, &c, Backend::Onnx(OnnxDtype::Q8)),
            PathBuf::from("/srv/data/models/embeddinggemma-2-onnx-q8")
        );
        let env = |k: &str| (k == "MARGIN_EMBED_MODEL_DIR").then(|| "/m".to_string());
        assert_eq!(
            model_dir(&env, &c, Backend::Onnx(OnnxDtype::Fp16)),
            PathBuf::from("/m")
        );
        assert_eq!(
            extraction_file("pdfium+tesseract"),
            PathBuf::from(crate::corpus::EXTRACTED_FILE)
        );
        assert_eq!(
            extraction_file("other+ocr"),
            PathBuf::from("recorded/extracted-other+ocr.json")
        );
    }

    #[test]
    fn the_linux_name_comes_from_os_release() {
        assert_eq!(
            os_release_name(
                "NAME=\"Ubuntu\"\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\n"
            )
            .as_deref(),
            Some("Ubuntu 24.04.3 LTS")
        );
        assert_eq!(
            os_release_name("NAME=Debian\nVERSION_ID='13'\n").as_deref(),
            Some("Debian 13")
        );
        assert_eq!(os_release_name(""), None);
        assert!(!os_version().is_empty());
        assert_eq!(reader_name(), "pdfium+tesseract");
    }

    #[test]
    fn the_runner_config_parses() {
        let cfg = RunnerConfig::load(&crate::golden::evals_dir().join("runner.toml")).unwrap();
        assert!(cfg.window_tokens >= 100_000);
        assert!(cfg.models.anthropic_small.starts_with("claude-"));
        let prices: crate::Prices = toml::from_str(
            &std::fs::read_to_string(crate::golden::evals_dir().join("prices.toml")).unwrap(),
        )
        .unwrap();
        for m in [&cfg.models.anthropic_write, &cfg.models.anthropic_small] {
            assert!(prices.models.contains_key(m), "{m} has no price");
        }
    }

    #[test]
    fn models_follow_the_keys() {
        let cfg = RunnerConfig::load(&crate::golden::evals_dir().join("runner.toml")).unwrap();
        let none = margin_service::secrets::MemoryStore::new();
        assert!(models(&cfg, &none).is_err());
        let both = margin_service::secrets::MemoryStore::new()
            .with(OPENAI_KEY, "sk-test")
            .with(ANTHROPIC_KEY, "ak-test");
        let m = models(&cfg, &both).unwrap();
        assert_eq!(m.writer.provider(), "anthropic");
        let providers: Vec<&str> = m.small.iter().map(|s| s.provider()).collect();
        assert_eq!(providers, ["anthropic", "openai"], "a judge per provider");
        assert!(m.chat.is_some());
        let openai = margin_service::secrets::MemoryStore::new().with(OPENAI_KEY, "sk-test");
        let m = models(&cfg, &openai).unwrap();
        assert_eq!(m.writer.provider(), "openai");
        assert_eq!(m.small.len(), 1);
    }

    async fn serve(app: axum::Router) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    #[tokio::test]
    async fn the_claude_client_against_a_local_fake() {
        let app = axum::Router::new().route(
            "/v1/messages",
            axum::routing::post(|h: axum::http::HeaderMap, axum::Json(body): axum::Json<Value>| async move {
                assert_eq!(h["x-api-key"], "ak-test");
                assert_eq!(h["anthropic-version"], "2023-06-01");
                assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
                assert!(body["system"][0]["text"].as_str().unwrap().contains("JSON schema"));
                assert_eq!(body["messages"][0]["content"][0]["type"], "image");
                axum::Json(json!({
                    "content": [{"type": "text", "text": "{\"grade\": 2}"}],
                    "usage": {"input_tokens": 10, "cache_read_input_tokens": 90, "cache_creation_input_tokens": 0, "output_tokens": 5}
                }))
            }),
        );
        let base = serve(app).await;
        let m = AnthropicModel::new(&base, "ak-test", "claude-haiku-5-5");
        let mut req = ModelRequest::new("Grade it.", "Q");
        req.format = Some(("g".into(), json!({"type": "object"})));
        req.images = vec![margin_ai::Image {
            media_type: "image/png".into(),
            data: vec![1, 2, 3],
        }];
        let r = m.complete(&req).await.unwrap();
        assert_eq!(r.text, "{\"grade\": 2}");
        assert_eq!(
            (r.usage.input, r.usage.cached, r.usage.output),
            (100, 90, 5)
        );
        assert_eq!(m.id(), "anthropic:claude-haiku-5-5");
    }

    #[tokio::test]
    async fn the_openai_client_against_a_local_fake() {
        let app = axum::Router::new().route(
            "/responses",
            axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
                assert_eq!(body["instructions"], "Sys");
                assert_eq!(body["prompt_cache_key"], "judge:x");
                assert_eq!(body["model"], "gpt-test");
                let sse = [
                    json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "m", "content_index": 0, "delta": "he"}),
                    json!({"type": "response.output_text.delta", "output_index": 0, "item_id": "m", "content_index": 0, "delta": "llo"}),
                    json!({"type": "response.completed", "response": {"id": "r", "usage": {"input_tokens": 7, "output_tokens": 2, "input_tokens_details": {"cached_tokens": 3}}}}),
                ]
                .iter()
                .map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap()))
                .collect::<String>();
                ([("content-type", "text/event-stream")], sse)
            }),
        );
        let base = serve(app).await;
        let m = OpenAiModel::new(&base, "sk-test", "gpt-test");
        let mut req = ModelRequest::new("Sys", "User");
        req.cache_key = Some("judge:x".into());
        let r = m.complete(&req).await.unwrap();
        assert_eq!(r.text, "hello");
        assert_eq!((r.usage.input, r.usage.cached, r.usage.output), (7, 3, 2));
    }
}
