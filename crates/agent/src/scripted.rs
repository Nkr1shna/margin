//! A scripted model for tests: replies queued in order, every request recorded. It
//! emits the neutral `ModelEvent`s a provider client produces (`margin_ai::model`), and
//! answers plain text calls (`TextModel`) from the same queue.
//!
//! The helpers that build Responses items (`message_item`, `function_call_item`,
//! `reasoning_item`) are what an OpenAI step stores; `text_reply` and
//! `tool_calls_reply` wrap them in the events the Responses client would emit.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use margin_ai::{
    AiError, CallOptions, Delta, InputPart, ItemView, ModelEvent, ModelInfo, ModelItem,
    ModelRequest, ModelStream, Next, Provider, ProviderKind, ResponseRequest, RouteModels,
    StartKind, TextModel, TextOutput, responses_request,
};
use margin_shared::Usage;
use serde_json::{Value, json};

use crate::BoxFuture;

/// One queued reply.
pub enum Scripted {
    /// Stream these events, then end.
    Events(Vec<ModelEvent>),
    /// Fail before streaming (e.g. `AiError::Http { status: 429, .. }`).
    Fail(AiError),
    /// Stream these events, then end with this error (a connection that dropped).
    EventsThenError(Vec<ModelEvent>, AiError),
    /// Wait this long before replying with the inner reply.
    Delayed(Duration, Box<Scripted>),
    /// Stream these events and then never finish (for Stop while streaming).
    Hang(Vec<ModelEvent>),
    /// Stream these events, then panic while the next one is read (a bug in the loop or
    /// a tool, for what the service does with a turn that ended that way).
    EventsThenPanic(Vec<ModelEvent>),
}

/// A request the scripted model got, with the model it was made on when the caller
/// said (`ScriptedModel::on_models`).
#[derive(Debug, Clone)]
pub struct Recorded {
    pub request: ModelRequest,
    pub model: Option<String>,
}

/// See the module docs. `Default` is an empty script; a call with nothing queued fails
/// with `AiError::Incomplete("the scripted model has no reply queued")`.
#[derive(Default)]
pub struct ScriptedModel {
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<Recorded>>,
    text_calls: Mutex<Vec<(String, CallOptions)>>,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = Scripted>) -> Self {
        Self {
            script: Mutex::new(replies.into_iter().collect()),
            ..Default::default()
        }
    }

    pub fn push(&self, reply: Scripted) {
        self.script.lock().unwrap().push_back(reply);
    }

    /// Every agent step requested so far, in order, as neutral requests.
    pub fn model_requests(&self) -> Vec<ModelRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| r.request.clone())
            .collect()
    }

    /// Every agent step requested so far, rendered as the Responses requests an OpenAI
    /// client would send (`model` is the recorded model, `None` when unknown).
    pub fn requests(&self) -> Vec<ResponseRequest> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|r| {
                let mut rendered = responses_request(&r.request, "");
                rendered.model = r.model.clone();
                rendered
            })
            .collect()
    }

    /// Every plain text call so far: its instructions and options.
    pub fn text_calls(&self) -> Vec<(String, CallOptions)> {
        self.text_calls.lock().unwrap().clone()
    }

    /// Replies not yet used.
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }

    /// This model as a provider configured with `models`: the requests it records carry
    /// the model of their route.
    pub fn on_models(self: &Arc<Self>, models: RouteModels) -> Arc<dyn Provider> {
        Arc::new(OnModels {
            inner: self.clone(),
            models,
        })
    }

    fn reply(&self, request: ModelRequest, model: Option<String>) -> Option<Scripted> {
        self.requests
            .lock()
            .unwrap()
            .push(Recorded { request, model });
        self.script.lock().unwrap().pop_front()
    }
}

/// Plays one reply.
async fn play(reply: Option<Scripted>) -> Result<ModelStream, AiError> {
    let mut reply = reply
        .ok_or_else(|| AiError::Incomplete("the scripted model has no reply queued".into()))?;
    loop {
        match reply {
            Scripted::Delayed(wait, inner) => {
                tokio::time::sleep(wait).await;
                reply = *inner;
            }
            Scripted::Fail(e) => return Err(e),
            Scripted::Events(events) => {
                return Ok(futures_util::stream::iter(events.into_iter().map(Ok)).boxed());
            }
            Scripted::EventsThenError(events, e) => {
                let items = events.into_iter().map(Ok).chain(std::iter::once(Err(e)));
                return Ok(futures_util::stream::iter(items).boxed());
            }
            Scripted::Hang(events) => {
                let head = futures_util::stream::iter(events.into_iter().map(Ok));
                return Ok(head.chain(futures_util::stream::pending()).boxed());
            }
            Scripted::EventsThenPanic(events) => {
                let head = futures_util::stream::iter(events.into_iter().map(Ok));
                let panic = futures_util::stream::poll_fn(
                    |_| -> std::task::Poll<Option<Result<ModelEvent, AiError>>> {
                        panic!("the scripted model panicked")
                    },
                );
                return Ok(head.chain(panic).boxed());
            }
        }
    }
}

impl Provider for ScriptedModel {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        let reply = self.reply(req, None);
        Box::pin(play(reply))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>> {
        Box::pin(async { Ok(Vec::new()) })
    }
}

/// `ScriptedModel::on_models`.
struct OnModels {
    inner: Arc<ScriptedModel>,
    models: RouteModels,
}

impl Provider for OnModels {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAi
    }

    fn stream(&self, req: ModelRequest) -> BoxFuture<'_, Result<ModelStream, AiError>> {
        let model = self.models.get(req.opts.route).to_string();
        let reply = self.inner.reply(req, Some(model));
        Box::pin(play(reply))
    }

    fn list_models(&self) -> BoxFuture<'_, Result<Vec<ModelInfo>, AiError>> {
        self.inner.list_models()
    }
}

impl TextModel for ScriptedModel {
    /// The next reply's text (its text deltas, or its finished text item), streamed to
    /// `on_delta`.
    fn stream_parts<'a>(
        &'a self,
        instructions: &'a str,
        _input: &'a [InputPart],
        opts: &'a CallOptions,
        mut on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<TextOutput, AiError>> {
        self.text_calls
            .lock()
            .unwrap()
            .push((instructions.to_string(), opts.clone()));
        let reply = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            let mut stream = play(reply).await?;
            let mut text = String::new();
            while let Some(event) = stream.next().await {
                match event? {
                    ModelEvent::Delta {
                        delta: Delta::Text(delta),
                        ..
                    } => {
                        on_delta(&delta);
                        text.push_str(&delta);
                    }
                    ModelEvent::Done { item, .. } => {
                        if let ItemView::Text { text: done, .. } = item.view {
                            text = done;
                        }
                    }
                    ModelEvent::Completed {
                        usage,
                        model,
                        stop_reason,
                        ..
                    } => {
                        return Ok(TextOutput {
                            text,
                            usage,
                            model,
                            stop_reason,
                        });
                    }
                    ModelEvent::Incomplete { reason, .. } => {
                        return Err(AiError::Incomplete(reason));
                    }
                    ModelEvent::Failed { message, .. } => {
                        return Err(AiError::Incomplete(message));
                    }
                    _ => {}
                }
            }
            Err(AiError::Network("the scripted reply ended early".into()))
        })
    }
}

/// Usage of one scripted call.
pub fn usage(input_tokens: u64, output_tokens: u64) -> Usage {
    Usage {
        input_tokens,
        cached_input_tokens: 0,
        output_tokens,
        reasoning_tokens: 0,
        model_calls: 1,
        ..Default::default()
    }
}

/// A `message` output item with this text, complete.
pub fn message_item(id: &str, text: &str) -> Value {
    json!({
        "type": "message",
        "id": id,
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    })
}

/// A `function_call` output item, complete.
pub fn function_call_item(id: &str, call_id: &str, name: &str, arguments: &str) -> Value {
    json!({
        "type": "function_call",
        "id": id,
        "call_id": call_id,
        "name": name,
        "arguments": arguments,
        "status": "completed",
    })
}

/// A `reasoning` output item with a summary and opaque encrypted content.
pub fn reasoning_item(id: &str, summary: &str) -> Value {
    json!({
        "type": "reasoning",
        "id": id,
        "summary": [{"type": "summary_text", "text": summary}],
        "encrypted_content": format!("enc-{id}"),
    })
}

/// `ModelEvent::Done` of a finished text item stored as `stored`.
pub fn text_done(index: u32, text: &str, stored: Value) -> ModelEvent {
    ModelEvent::Done {
        index,
        item: ModelItem {
            stored: vec![stored],
            view: ItemView::Text {
                text: text.into(),
                citations: Vec::new(),
            },
        },
    }
}

/// `ModelEvent::Done` of a finished reasoning item.
pub fn reasoning_done(index: u32, summary: &str, stored: Value) -> ModelEvent {
    ModelEvent::Done {
        index,
        item: ModelItem {
            stored: vec![stored],
            view: ItemView::Reasoning {
                summary: summary.into(),
            },
        },
    }
}

/// `ModelEvent::Done` of a finished function call.
pub fn call_done(
    index: u32,
    call_id: &str,
    name: &str,
    arguments: &str,
    stored: Value,
) -> ModelEvent {
    ModelEvent::Done {
        index,
        item: ModelItem {
            stored: vec![stored],
            view: ItemView::ToolCall {
                call_id: call_id.into(),
                name: name.into(),
                arguments: arguments.into(),
            },
        },
    }
}

/// `ModelEvent::Completed` of a finished answer. The model is left empty (a server that
/// doesn't say), so traces keep the model the call was made on.
pub fn completed(usage: Usage) -> ModelEvent {
    ModelEvent::Completed {
        usage,
        model: String::new(),
        stop_reason: "completed".into(),
        next: Next::Finished,
    }
}

/// The events of a response that answers with `text` (streamed in `chunks`).
pub fn text_reply(chunks: &[&str]) -> Vec<ModelEvent> {
    let text: String = chunks.concat();
    let mut events = vec![ModelEvent::Started {
        index: 0,
        kind: StartKind::Text,
    }];
    for chunk in chunks {
        events.push(ModelEvent::Delta {
            index: 0,
            delta: Delta::Text((*chunk).into()),
        });
    }
    events.push(text_done(0, &text, message_item("msg_1", &text)));
    events.push(completed(usage(100, 20)));
    events
}

/// The events of a response that calls tools: `(call_id, name, arguments JSON)` each,
/// arguments streamed in one delta.
pub fn tool_calls_reply(calls: &[(&str, &str, &str)]) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    for (i, (call_id, name, args)) in calls.iter().enumerate() {
        let index = i as u32;
        let id = format!("fc_{call_id}");
        events.push(ModelEvent::Started {
            index,
            kind: StartKind::ToolCall {
                call_id: (*call_id).into(),
                name: (*name).into(),
            },
        });
        events.push(ModelEvent::Delta {
            index,
            delta: Delta::Arguments((*args).into()),
        });
        events.push(call_done(
            index,
            call_id,
            name,
            args,
            function_call_item(&id, call_id, name, args),
        ));
    }
    events.push(completed(usage(100, 10)));
    events
}

#[cfg(test)]
mod tests {
    use super::*;
    use margin_ai::{ToolChoice, WebTools};
    use margin_shared::ReasoningEffort;
    use margin_shared::pipeline::Route;

    fn request() -> ModelRequest {
        ModelRequest {
            opts: CallOptions::new(Route::Chat).with_cache_key("chat"),
            system: "system".into(),
            history: vec![],
            tools: vec![],
            tool_choice: ToolChoice::Auto,
            web: WebTools::default(),
            effort: ReasoningEffort::Medium,
            compaction: None,
            parallel_tool_calls: true,
        }
    }

    #[tokio::test]
    async fn replies_in_order_and_records_requests() {
        let model = ScriptedModel::new([
            Scripted::Events(text_reply(&["Hel", "lo"])),
            Scripted::Fail(AiError::Http {
                status: 429,
                message: "slow down".into(),
                retry_after: Some(Duration::from_secs(1)),
                code: None,
            }),
        ]);
        let events: Vec<_> = model.stream(request()).await.unwrap().collect().await;
        assert!(matches!(
            events.last(),
            Some(Ok(ModelEvent::Completed { .. }))
        ));
        assert!(matches!(
            model.stream(request()).await,
            Err(AiError::Http { status: 429, .. })
        ));
        assert!(matches!(
            model.stream(request()).await,
            Err(AiError::Incomplete(_))
        ));
        assert_eq!(model.requests().len(), 3);
        assert_eq!(
            model.requests()[0].prompt_cache_key.as_deref(),
            Some("chat")
        );
        assert_eq!(model.model_requests()[0].system, "system");
        assert_eq!(model.remaining(), 0);
    }

    #[tokio::test]
    async fn records_the_model_of_the_route() {
        let model = Arc::new(ScriptedModel::new([Scripted::Events(text_reply(&["a"]))]));
        let configured = model.on_models(RouteModels {
            chat: "chat-model".into(),
            write: "w".into(),
            small: "s".into(),
        });
        let _ = configured.stream(request()).await.unwrap();
        assert_eq!(model.requests()[0].model.as_deref(), Some("chat-model"));
    }

    #[tokio::test]
    async fn answers_text_calls_from_the_queue() {
        let model = ScriptedModel::new([Scripted::Events(text_reply(&["Sum", "mary"]))]);
        let mut seen = String::new();
        let out = model
            .stream_parts(
                "instructions",
                &[],
                &CallOptions::new(Route::Small),
                Box::new(|d| seen.push_str(d)),
            )
            .await
            .unwrap();
        assert_eq!((out.text.as_str(), seen.as_str()), ("Summary", "Summary"));
        assert_eq!(model.text_calls()[0].1.route, Route::Small);
    }

    #[tokio::test]
    async fn errors_after_events() {
        let model = ScriptedModel::new([Scripted::EventsThenError(
            tool_calls_reply(&[("call_1", "list_topics", "{}")]),
            AiError::Network("reset".into()),
        )]);
        let events: Vec<_> = model.stream(request()).await.unwrap().collect().await;
        assert!(matches!(events.last(), Some(Err(AiError::Network(_)))));
        assert!(events[..events.len() - 1].iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn hangs_after_its_events() {
        let model = ScriptedModel::new([Scripted::Hang(text_reply(&["a"])[..2].to_vec())]);
        let mut stream = model.stream(request()).await.unwrap();
        assert!(stream.next().await.is_some());
        assert!(stream.next().await.is_some());
        let more = tokio::time::timeout(Duration::from_millis(20), stream.next()).await;
        assert!(more.is_err(), "the stream should still be waiting");
    }
}
