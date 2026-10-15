//! The provider routes (`docs/agent-providers-brief.md` D9): the models each provider
//! offers, with their windows, and the "Sign in with ChatGPT" flow, which runs on the
//! service Mac (its OAuth callback is a loopback URL there).
//!
//! - `GET routes::providers_models(provider)` → `Vec<ModelInfo>`; 409 `not_configured`
//!   when the provider has no credential.
//! - `POST routes::PROVIDERS_CHATGPT_START` → `ChatGptStart`.
//! - `GET routes::PROVIDERS_CHATGPT` → `ChatGptStatus`.
//! - `DELETE routes::PROVIDERS_CHATGPT` → 204: signs out (deletes the tokens).
//!
//! Built by area P-service.

use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use margin_shared::{ChatGptStart, ChatGptStatus, ModelInfo, Provider};

use crate::error::AppResult;
use crate::state::AppState;

pub async fn models(
    State(state): State<AppState>,
    Path(provider): Path<Provider>,
) -> AppResult<Json<Vec<ModelInfo>>> {
    let _ = (state, provider);
    unimplemented!("providers, P-service")
}

pub async fn chatgpt_start(State(state): State<AppState>) -> AppResult<Json<ChatGptStart>> {
    let _ = state;
    unimplemented!("providers, P-service")
}

pub async fn chatgpt_status(State(state): State<AppState>) -> AppResult<Json<ChatGptStatus>> {
    let _ = state;
    unimplemented!("providers, P-service")
}

pub async fn chatgpt_sign_out(State(state): State<AppState>) -> AppResult<StatusCode> {
    let _ = state;
    unimplemented!("providers, P-service")
}
