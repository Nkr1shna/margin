//! `margin-mcp`: Claude Code's door into Margin while Margin is the knowledge base for
//! building Margin (`docs/plan-dogfood.md`). A single-user adapter over the context API
//! (`margin_shared::context`, `routes::CONTEXT_*`) behind an agent token (`margin-mcp
//! token`, the agent's ceiling) or the person's own device token (`margin-mcp login`).

pub mod client;
pub mod config;
pub mod ingest;
pub mod login;
pub mod serve;
pub mod token;
