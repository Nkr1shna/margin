//! The Margin service: an axum server on the owner's Mac with embedded SurrealDB,
//! GitHub sign-in, background generation jobs, a live event socket and signed media.
//! See `PLAN.md` ("Service") and `margin_shared::routes`.

pub mod agent_tools;
pub mod alerts;
pub mod ask;
pub mod auth;
pub mod chats;
pub mod config;
pub mod context_api;
pub mod db;
pub mod devices;
pub mod embedding;
pub mod engines;
pub mod error;
pub mod events;
pub mod folders;
pub mod ingest;
pub mod library;
pub mod media;
pub mod pipeline;
pub mod providers;
pub mod restore;
pub mod retrieval;
pub mod search;
pub mod secrets;
pub mod server;
pub mod settings;
pub mod sources;
pub mod state;
pub mod trace;
pub mod turns;
pub mod uploads;
pub mod version;
pub mod video;
pub mod workspaces;
