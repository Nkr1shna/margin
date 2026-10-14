//! The Margin service: an axum server on the owner's Mac with embedded SurrealDB,
//! GitHub sign-in, background generation jobs, a live event socket and signed media.
//! See `PLAN.md` ("Service") and `margin_shared::routes`.

pub mod ask;
pub mod auth;
pub mod config;
pub mod db;
pub mod embedding;
pub mod engines;
pub mod error;
pub mod events;
pub mod library;
pub mod media;
pub mod search;
pub mod secrets;
pub mod server;
pub mod settings;
pub mod sources;
pub mod state;
pub mod uploads;
pub mod video;
