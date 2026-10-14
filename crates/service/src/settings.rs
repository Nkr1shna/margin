//! Settings (the API key goes to the Keychain and never comes back) and tool status.

use axum::Json;
use axum::extract::State;
use margin_shared::{Event, Settings, SettingsPatch, ToolStatus};

use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::secrets::OPENAI_API_KEY;
use crate::state::{AppState, SETTINGS_ID};

pub async fn get_settings(State(state): State<AppState>) -> AppResult<Json<Settings>> {
    Ok(Json(state.settings().await?))
}

fn check_dimension(name: &str, v: u32) -> AppResult<()> {
    if !(16..=7680).contains(&v) || !v.is_multiple_of(2) {
        return Err(AppError::bad_request(format!(
            "The render {name} must be an even number from 16 to 7680."
        )));
    }
    Ok(())
}

pub async fn patch_settings(
    State(state): State<AppState>,
    ApiJson(patch): ApiJson<SettingsPatch>,
) -> AppResult<Json<Settings>> {
    {
        let _w = state.write_lock.lock().await;
        let mut row = state.settings_row().await?;
        if let Some(url) = &patch.openai_base_url {
            let url = url.trim().trim_end_matches('/');
            if !(url.starts_with("https://") || url.starts_with("http://"))
                || reqwest::Url::parse(url).is_err()
            {
                return Err(AppError::bad_request(
                    "The base URL must be an http(s) URL, like https://api.openai.com/v1.",
                ));
            }
            row.openai_base_url = url.to_string();
        }
        if let Some(model) = &patch.openai_model {
            row.openai_model = model.trim().to_string();
        }
        if let Some(voice) = &patch.voice {
            row.voice = voice
                .as_ref()
                .map(|v| v.trim().to_string())
                .filter(|v| !v.is_empty());
        }
        if let Some(w) = patch.render_width {
            check_dimension("width", w)?;
            row.render_width = w;
        }
        if let Some(h) = patch.render_height {
            check_dimension("height", h)?;
            row.render_height = h;
        }
        if let Some(fps) = patch.render_fps {
            if !(1..=120).contains(&fps) {
                return Err(AppError::bad_request(
                    "The frame rate must be from 1 to 120.",
                ));
            }
            row.render_fps = fps;
        }
        match patch.openai_api_key.as_deref().map(str::trim) {
            Some("") => state.secret_delete(OPENAI_API_KEY).await?,
            Some(key) => state.secret_set(OPENAI_API_KEY, key.to_string()).await?,
            None => {}
        }
        state.store.put(db::SETTINGS, SETTINGS_ID, &row).await?;
    }
    let settings = state.settings().await?;
    state.emit(Event::SettingsChanged {
        settings: settings.clone(),
    });
    Ok(Json(settings))
}

pub async fn tools(State(state): State<AppState>) -> Json<Vec<ToolStatus>> {
    Json(state.renderer.detect_tools().await)
}
