//! The service's seams to `margin-ai` and `margin-video`. Jobs call these traits; the
//! production implementations forward to the crates, and tests plug in fakes.

use std::future::Future;
use std::pin::Pin;

use margin_ai::{AiError, LessonContext, ResponsesClient, SceneRequest};
use margin_shared::{AskRequest, Outline, ToolStatus};
use margin_video::{RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Where and how to call the Responses API. Built per job from Settings + Keychain.
#[derive(Clone)]
pub struct AiConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

impl std::fmt::Debug for AiConfig {
    // Never print the key.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AiConfig")
            .field("base_url", &self.base_url)
            .field("model", &self.model)
            .finish()
    }
}

pub trait Generator: Send + Sync + 'static {
    fn propose_outline<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a AskRequest,
        existing_subjects: &'a [String],
    ) -> BoxFuture<'a, Result<Outline, AiError>>;

    fn write_lesson<'a>(
        &'a self,
        ai: &'a AiConfig,
        ctx: &'a LessonContext,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<String, AiError>>;

    fn write_scene<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a SceneRequest,
    ) -> BoxFuture<'a, Result<String, AiError>>;
}

pub trait Renderer: Send + Sync + 'static {
    fn detect_tools(&self) -> BoxFuture<'_, Vec<ToolStatus>>;

    fn render<'a>(
        &'a self,
        job: &'a RenderJob,
        config: &'a RenderConfig,
        on_progress: Box<dyn FnMut(RenderProgress) + Send + 'a>,
    ) -> BoxFuture<'a, Result<RenderOutput, RenderError>>;
}

/// `margin_ai` against the configured Responses API.
pub struct AiGenerator;

fn client(ai: &AiConfig) -> ResponsesClient {
    ResponsesClient::new(ai.base_url.clone(), ai.api_key.clone(), ai.model.clone())
}

impl Generator for AiGenerator {
    fn propose_outline<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a AskRequest,
        existing_subjects: &'a [String],
    ) -> BoxFuture<'a, Result<Outline, AiError>> {
        Box::pin(
            async move { margin_ai::propose_outline(&client(ai), req, existing_subjects).await },
        )
    }

    fn write_lesson<'a>(
        &'a self,
        ai: &'a AiConfig,
        ctx: &'a LessonContext,
        on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move { margin_ai::write_lesson(&client(ai), ctx, on_delta).await })
    }

    fn write_scene<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a SceneRequest,
    ) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move { margin_ai::write_scene(&client(ai), req).await })
    }
}

/// `margin_video`'s ManimGX + FluidAudio pipeline.
pub struct VideoRenderer;

impl Renderer for VideoRenderer {
    fn detect_tools(&self) -> BoxFuture<'_, Vec<ToolStatus>> {
        Box::pin(margin_video::detect_tools())
    }

    fn render<'a>(
        &'a self,
        job: &'a RenderJob,
        config: &'a RenderConfig,
        on_progress: Box<dyn FnMut(RenderProgress) + Send + 'a>,
    ) -> BoxFuture<'a, Result<RenderOutput, RenderError>> {
        Box::pin(margin_video::render(job, config, on_progress))
    }
}
