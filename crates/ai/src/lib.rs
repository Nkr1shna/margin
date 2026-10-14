//! Text generation for Margin through an OpenAI-compatible **Responses API**
//! (`POST {base_url}/responses`). No other API shape is supported.
//!
//! The service owns configuration and the API key; this crate is stateless and
//! knows nothing about storage.
//!
//! STUB: signatures are the contract with `margin-service`. Bodies are filled in
//! by the `ai` implementation task.

use margin_shared::{AskRequest, Level, Outline, OutlineLesson};

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    /// The API answered with a non-2xx status. `message` is the API's error message
    /// when it sent one.
    #[error("the model API returned {status}: {message}")]
    Http { status: u16, message: String },
    #[error("couldn't reach the model API: {0}")]
    Network(String),
    /// The response could not be parsed, or structured output didn't match the schema.
    #[error("the model's answer couldn't be read: {0}")]
    Parse(String),
    /// The model refused or the response ended as `incomplete`/`failed`.
    #[error("the model didn't finish: {0}")]
    Incomplete(String),
}

/// A configured Responses API endpoint. Cheap to clone.
#[derive(Clone)]
pub struct ResponsesClient {
    _private: (),
}

impl ResponsesClient {
    /// `base_url` like "https://api.openai.com/v1" (no trailing `/responses`).
    pub fn new(base_url: impl Into<String>, api_key: impl Into<String>, model: impl Into<String>) -> Self {
        let _ = (base_url.into(), api_key.into(), model.into());
        todo!()
    }

    /// Streams a plain-text answer (`stream: true`), calling `on_delta` with each
    /// `response.output_text.delta`. Returns the full text.
    pub async fn stream_text(
        &self,
        instructions: &str,
        input: &str,
        on_delta: impl FnMut(&str) + Send,
    ) -> Result<String, AiError> {
        let _ = (instructions, input, on_delta);
        todo!()
    }

    /// Non-streaming structured output (`text.format = {type: "json_schema", strict: true}`),
    /// deserialized into `T`.
    pub async fn structured<T: serde::de::DeserializeOwned>(
        &self,
        instructions: &str,
        input: &str,
        schema_name: &str,
        schema: serde_json::Value,
    ) -> Result<T, AiError> {
        let _ = (instructions, input, schema_name, schema);
        todo!()
    }
}

/// Proposes a course outline. `existing_subjects` are the titles of root topics, so the
/// model can file the topic under one of them (it may also invent a new subject).
/// The result has exactly `req.lesson_count` lessons.
pub async fn propose_outline(
    client: &ResponsesClient,
    req: &AskRequest,
    existing_subjects: &[String],
) -> Result<Outline, AiError> {
    let _ = (client, req, existing_subjects);
    todo!()
}

/// Everything needed to write one lesson of an accepted outline.
#[derive(Debug, Clone)]
pub struct LessonContext {
    pub subject_title: String,
    pub topic_title: String,
    /// The learner's original request.
    pub prompt: String,
    pub level: Level,
    pub with_quiz: bool,
    /// The whole outline, so the lesson can reference neighbours.
    pub outline: Vec<OutlineLesson>,
    /// Index into `outline` of the lesson to write.
    pub index: usize,
}

/// Streams a lesson body as Markdown (see `Lesson::body_md` for conventions).
pub async fn write_lesson(
    client: &ResponsesClient,
    ctx: &LessonContext,
    on_delta: impl FnMut(&str) + Send,
) -> Result<String, AiError> {
    let _ = (client, ctx, on_delta);
    todo!()
}

#[derive(Debug, Clone)]
pub struct SceneRequest {
    pub lesson_title: String,
    pub lesson_body_md: String,
    /// A previous scene and the error ManimGX reported for it, to repair instead of
    /// starting over.
    pub previous_attempt: Option<FailedScene>,
}

#[derive(Debug, Clone)]
pub struct FailedScene {
    pub code: String,
    pub error: String,
}

/// Writes a ManimGX scene file (Python) explaining the lesson. Contract with
/// `margin-video`:
/// - `import manimgx as m` and nothing that needs the network or files outside the
///   working directory;
/// - exactly one scene class, named `LessonScene`, deriving `m.Scene`;
/// - narration only through `self.say("…[words]…", animations…)`;
/// - no `voice` attribute (the renderer installs the voice);
/// - returns only code, no Markdown fences.
pub async fn write_scene(client: &ResponsesClient, req: &SceneRequest) -> Result<String, AiError> {
    let _ = (client, req);
    todo!()
}
