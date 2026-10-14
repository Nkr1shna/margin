//! The seam between the runner's suites and a model: one request in, text and usage
//! out. The opt-in runner plugs in the real OpenAI and Claude clients (`live.rs`);
//! every test plugs in [`ScriptedModel`]. Nothing in a test reaches a model API.

use std::sync::Mutex;

use margin_ai::Image;
use margin_service::engines::BoxFuture;
use margin_shared::context::TraceUsage;
use serde::de::DeserializeOwned;

/// One call.
#[derive(Debug, Clone, PartialEq)]
pub struct ModelRequest {
    /// The constant part (instructions, rubric): cached across calls.
    pub system: String,
    /// The variable part.
    pub user: String,
    /// Pictures sent after `user`.
    pub images: Vec<Image>,
    pub max_output_tokens: u32,
    /// OpenAI `prompt_cache_key`; Claude gets a cache breakpoint after `system`.
    pub cache_key: Option<String>,
    /// A strict JSON schema the answer must follow (name, schema): OpenAI's
    /// `text.format`; Claude is told in the instructions.
    pub format: Option<(String, serde_json::Value)>,
}

impl ModelRequest {
    pub fn new(system: impl Into<String>, user: impl Into<String>) -> Self {
        Self {
            system: system.into(),
            user: user.into(),
            images: Vec::new(),
            max_output_tokens: 4_000,
            cache_key: None,
            format: None,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelReply {
    pub text: String,
    pub usage: TraceUsage,
}

pub trait EvalModel: Send + Sync {
    /// "openai:gpt-6-luna", "anthropic:claude-haiku-5-5": recorded with every grade.
    fn id(&self) -> String;
    /// "openai" or "anthropic".
    fn provider(&self) -> &str;
    /// The model id, as `evals/prices.toml` keys it.
    fn model(&self) -> &str;
    fn complete<'a>(&'a self, req: &'a ModelRequest) -> BoxFuture<'a, anyhow::Result<ModelReply>>;
}

/// The JSON object in a model's answer: the whole text, or the outermost `{…}` in it
/// (models sometimes wrap JSON in a code fence or a sentence).
pub fn parse_json<T: DeserializeOwned>(text: &str) -> anyhow::Result<T> {
    if let Ok(v) = serde_json::from_str(text.trim()) {
        return Ok(v);
    }
    let start = text.find('{');
    let end = text.rfind('}');
    match (start, end) {
        (Some(s), Some(e)) if e > s => serde_json::from_str(&text[s..=e]).map_err(|err| {
            anyhow::anyhow!("the model's answer isn't the JSON asked for ({err}): {}", short(text))
        }),
        _ => anyhow::bail!("the model's answer has no JSON object: {}", short(text)),
    }
}

fn short(text: &str) -> String {
    text.chars().take(200).collect()
}

/// A fake model for tests: answers each request with `reply(request)` and keeps the
/// requests.
pub struct ScriptedModel {
    id: String,
    provider: String,
    model: String,
    #[allow(clippy::type_complexity)]
    reply: Box<dyn Fn(&ModelRequest) -> anyhow::Result<String> + Send + Sync>,
    pub requests: Mutex<Vec<ModelRequest>>,
}

impl ScriptedModel {
    pub fn new(
        provider: &str,
        model: &str,
        reply: impl Fn(&ModelRequest) -> anyhow::Result<String> + Send + Sync + 'static,
    ) -> Self {
        Self {
            id: format!("{provider}:{model}"),
            provider: provider.into(),
            model: model.into(),
            reply: Box::new(reply),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn calls(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
}

impl EvalModel for ScriptedModel {
    fn id(&self) -> String {
        self.id.clone()
    }
    fn provider(&self) -> &str {
        &self.provider
    }
    fn model(&self) -> &str {
        &self.model
    }
    fn complete<'a>(&'a self, req: &'a ModelRequest) -> BoxFuture<'a, anyhow::Result<ModelReply>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(req.clone());
            let text = (self.reply)(req)?;
            Ok(ModelReply {
                usage: TraceUsage {
                    input: (req.system.len() + req.user.len()) as u64 / 4,
                    output: text.len() as u64 / 4,
                    ..Default::default()
                },
                text,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct G {
        grade: u8,
    }

    #[test]
    fn json_is_found_inside_chatter() {
        assert_eq!(parse_json::<G>("{\"grade\": 2}").unwrap(), G { grade: 2 });
        assert_eq!(
            parse_json::<G>("Sure:\n```json\n{\"grade\": 3}\n```").unwrap(),
            G { grade: 3 }
        );
        assert!(parse_json::<G>("no json").is_err());
        assert!(parse_json::<G>("{\"grade\": \"x\"}").is_err());
    }

    #[tokio::test]
    async fn the_scripted_model_records_requests() {
        let m = ScriptedModel::new("openai", "fake", |r| Ok(format!("echo {}", r.user)));
        let reply = m.complete(&ModelRequest::new("sys", "hi")).await.unwrap();
        assert_eq!(reply.text, "echo hi");
        assert_eq!(m.calls(), 1);
        assert_eq!(m.id(), "openai:fake");
    }
}
