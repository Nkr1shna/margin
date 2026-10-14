//! A scripted model for tests: replies queued in order, every request recorded. It
//! emits the same typed events `margin_ai` parses from the real stream.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::Duration;

use futures_util::StreamExt;
use margin_ai::{AiError, ResponseRequest, ResponseStream, StreamEvent};
use margin_shared::Usage;
use serde_json::{Value, json};

use crate::{BoxFuture, ModelClient};

/// One queued reply.
pub enum Scripted {
    /// Stream these events, then end.
    Events(Vec<StreamEvent>),
    /// Fail before streaming (e.g. `AiError::Http { status: 429, .. }`).
    Fail(AiError),
    /// Stream these events, then end with this error (a connection that dropped).
    EventsThenError(Vec<StreamEvent>, AiError),
    /// Wait this long before replying with the inner reply.
    Delayed(Duration, Box<Scripted>),
    /// Stream these events and then never finish (for Stop while streaming).
    Hang(Vec<StreamEvent>),
}

/// See the module docs. `Default` is an empty script; a call with nothing queued fails
/// with `AiError::Incomplete("the scripted model has no reply queued")`.
#[derive(Default)]
pub struct ScriptedModel {
    script: Mutex<VecDeque<Scripted>>,
    requests: Mutex<Vec<ResponseRequest>>,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = Scripted>) -> Self {
        Self {
            script: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }

    pub fn push(&self, reply: Scripted) {
        self.script.lock().unwrap().push_back(reply);
    }

    /// Every request made so far, in order.
    pub fn requests(&self) -> Vec<ResponseRequest> {
        self.requests.lock().unwrap().clone()
    }

    /// Replies not yet used.
    pub fn remaining(&self) -> usize {
        self.script.lock().unwrap().len()
    }
}

impl ModelClient for ScriptedModel {
    fn stream(&self, req: ResponseRequest) -> BoxFuture<'_, Result<ResponseStream, AiError>> {
        self.requests.lock().unwrap().push(req);
        let reply = self.script.lock().unwrap().pop_front();
        Box::pin(async move {
            let mut reply = reply.ok_or_else(|| {
                AiError::Incomplete("the scripted model has no reply queued".into())
            })?;
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
                }
            }
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

/// The events of a response that answers with `text` (streamed in `chunks`).
pub fn text_reply(chunks: &[&str]) -> Vec<StreamEvent> {
    let text: String = chunks.concat();
    let mut events = vec![StreamEvent::OutputItemAdded {
        output_index: 0,
        item: json!({"type": "message", "id": "msg_1", "status": "in_progress", "role": "assistant", "content": []}),
    }];
    for chunk in chunks {
        events.push(StreamEvent::OutputTextDelta {
            output_index: 0,
            item_id: "msg_1".into(),
            content_index: 0,
            delta: (*chunk).into(),
        });
    }
    events.push(StreamEvent::OutputItemDone {
        output_index: 0,
        item: message_item("msg_1", &text),
    });
    events.push(StreamEvent::Completed {
        response_id: "resp_text".into(),
        usage: usage(100, 20),
    });
    events
}

/// The events of a response that calls tools: `(call_id, name, arguments JSON)` each,
/// arguments streamed in one delta.
pub fn tool_calls_reply(calls: &[(&str, &str, &str)]) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    for (i, (call_id, name, args)) in calls.iter().enumerate() {
        let index = i as u32;
        let id = format!("fc_{call_id}");
        events.push(StreamEvent::OutputItemAdded {
            output_index: index,
            item: json!({"type": "function_call", "id": id, "call_id": call_id, "name": name, "arguments": "", "status": "in_progress"}),
        });
        events.push(StreamEvent::FunctionCallArgumentsDelta {
            output_index: index,
            item_id: id.clone(),
            delta: (*args).into(),
        });
        events.push(StreamEvent::FunctionCallArgumentsDone {
            output_index: index,
            item_id: id.clone(),
            arguments: (*args).into(),
        });
        events.push(StreamEvent::OutputItemDone {
            output_index: index,
            item: function_call_item(&id, call_id, name, args),
        });
    }
    events.push(StreamEvent::Completed {
        response_id: "resp_tools".into(),
        usage: usage(100, 10),
    });
    events
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn replies_in_order_and_records_requests() {
        let model = ScriptedModel::new([
            Scripted::Events(text_reply(&["Hel", "lo"])),
            Scripted::Fail(AiError::Http {
                status: 429,
                message: "slow down".into(),
                retry_after: Some(Duration::from_secs(1)),
            }),
        ]);
        let req = ResponseRequest {
            prompt_cache_key: Some("chat".into()),
            ..Default::default()
        };
        let events: Vec<_> = model.stream(req.clone()).await.unwrap().collect().await;
        assert!(matches!(
            events.last(),
            Some(Ok(StreamEvent::Completed { .. }))
        ));
        assert!(matches!(
            model.stream(req.clone()).await,
            Err(AiError::Http { status: 429, .. })
        ));
        assert!(matches!(
            model.stream(req).await,
            Err(AiError::Incomplete(_))
        ));
        assert_eq!(model.requests().len(), 3);
        assert_eq!(model.remaining(), 0);
    }

    #[tokio::test]
    async fn errors_after_events() {
        let model = ScriptedModel::new([Scripted::EventsThenError(
            tool_calls_reply(&[("call_1", "list_topics", "{}")]),
            AiError::Network("reset".into()),
        )]);
        let events: Vec<_> = model
            .stream(ResponseRequest::default())
            .await
            .unwrap()
            .collect()
            .await;
        assert!(matches!(events.last(), Some(Err(AiError::Network(_)))));
        assert!(events[..events.len() - 1].iter().all(Result::is_ok));
    }

    #[tokio::test]
    async fn hangs_after_its_events() {
        let model = ScriptedModel::new([Scripted::Hang(text_reply(&["a"])[..2].to_vec())]);
        let mut stream = model.stream(ResponseRequest::default()).await.unwrap();
        assert!(stream.next().await.is_some());
        assert!(stream.next().await.is_some());
        let more = tokio::time::timeout(Duration::from_millis(20), stream.next()).await;
        assert!(more.is_err(), "the stream should still be waiting");
    }
}
