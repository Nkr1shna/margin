//! The seam between the loop and the model API: `margin_ai::Provider`, one agent step
//! per call on neutral events (`docs/agent-providers-brief.md` D1). `ResponsesClient`
//! and `ClaudeClient` in production; tests use [`crate::ScriptedModel`].

pub use margin_ai::Provider as ModelClient;
