//! `margin-mcp`: Claude Code's door into Margin while Margin is the knowledge base for
//! building Margin (`docs/plan-dogfood.md`). A single-user adapter over the context API
//! (`margin_shared::context`, `routes::CONTEXT_*`) behind the owner's own device token.

pub mod client;
pub mod config;
pub mod ingest;
pub mod login;
pub mod serve;
