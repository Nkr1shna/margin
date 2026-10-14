//! The seam between the loop and the model API.

use margin_ai::{AiError, ResponseRequest, ResponseStream, ResponsesClient};

use crate::BoxFuture;

/// Makes one streamed model call. `margin_ai::ResponsesClient` in production; tests use
/// [`crate::ScriptedModel`].
pub trait ModelClient: Send + Sync + 'static {
    /// Starts the call. `Err` before any event (HTTP status, connection) is retried by
    /// the loop when `AiError::is_retryable`.
    fn stream(&self, req: ResponseRequest) -> BoxFuture<'_, Result<ResponseStream, AiError>>;
}

impl ModelClient for ResponsesClient {
    fn stream(&self, req: ResponseRequest) -> BoxFuture<'_, Result<ResponseStream, AiError>> {
        Box::pin(async move { self.stream_response(&req).await })
    }
}
