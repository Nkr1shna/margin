//! The real things the opt-in runner uses, and only the runner: the OpenAI Responses
//! and Claude Messages clients with the owner's keys from the service Mac's Keychain,
//! the Core ML embedder, and PDFKit/Vision extraction. Nothing here runs in tests or
//! CI: [`guard`] refuses when `CI` (or `MARGIN_EVAL_NO_LIVE`) is set, and every test
//! drives these clients against local fakes, never a provider.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use futures_util::StreamExt;
use margin_ai::{ResponseRequest, ResponsesClient, StreamEvent};
use margin_service::engines::BoxFuture;
use margin_shared::context::TraceUsage;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::model::{EvalModel, ModelReply, ModelRequest};

/// The Keychain entries the runner reads (the service's names).
pub const OPENAI_KEY: &str = margin_service::secrets::OPENAI_API_KEY;
pub const ANTHROPIC_KEY: &str = "anthropic-api-key";

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
    /// The service the traces are read from.
    #[serde(default)]
    pub service_url: Option<String>,
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
        "no model API key in the Keychain (openai-api-key or anthropic-api-key); set one in Margin's Settings on the service Mac",
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

/// The real embedder's vectors: EmbeddingGemma 2 on Core ML, pictures through its
/// image encoder (a PDF page from the render `extract` wrote under `media`).
#[cfg(target_os = "macos")]
pub struct LiveVectors {
    pub embedder: margin_embed::CoreMlEmbedder,
    pub corpus: PathBuf,
    pub media: PathBuf,
}

/// Where a document's page renders go.
pub fn media_dir(media: &Path, document: &str) -> PathBuf {
    media.join(document.replace('/', "_"))
}

#[cfg(target_os = "macos")]
impl LiveVectors {
    /// Loads the model from `MARGIN_EMBED_MODEL_DIR` or the service's data folder.
    pub fn load(corpus: &Path, media: &Path) -> anyhow::Result<Self> {
        let dir = match std::env::var_os("MARGIN_EMBED_MODEL_DIR") {
            Some(d) if !d.is_empty() => PathBuf::from(d),
            _ => margin_service::config::Config::load(&margin_service::config::config_path())?
                .embedding_model_dir(),
        };
        let files = margin_embed::ModelFiles::new(&dir);
        anyhow::ensure!(
            files.is_complete(),
            "the embedding model isn't downloaded in {} (download it in Margin's Settings)",
            dir.display()
        );
        Ok(Self {
            embedder: margin_embed::CoreMlEmbedder::load(&files)?,
            corpus: corpus.into(),
            media: media.into(),
        })
    }
}

#[cfg(target_os = "macos")]
impl crate::recorded::Vectors for LiveVectors {
    fn version(&self) -> &str {
        crate::recorded::embedder_version()
    }
    fn texts(
        &self,
        texts: &[String],
        purpose: &margin_embed::Purpose,
    ) -> anyhow::Result<Vec<Vec<f32>>> {
        use margin_embed::Embedder;
        Ok(self.embedder.embed(texts, purpose.clone())?)
    }
    fn picture(
        &self,
        doc: &crate::corpus::CorpusDoc,
        pic: &margin_embed::Picture,
        _text: &str,
    ) -> anyhow::Result<Vec<f32>> {
        use margin_embed::Embedder;
        let path = match pic.page {
            Some(_) if doc.kind == margin_shared::SourceKind::Pdf => {
                media_dir(&self.media, &doc.path).join(&pic.path)
            }
            _ => self.corpus.join(&doc.path),
        };
        Ok(self.embedder.embed_images(&[path], pic.tokens)?.remove(0))
    }
}

/// Reads every PDF and image of the corpus with the real readers (PDFKit, Vision OCR,
/// ImageIO), rendering PDF pages under `media`.
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
