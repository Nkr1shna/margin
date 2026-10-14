//! `/media/videos/{id}/{file}?exp&sig`: signed, range-capable video files.

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use margin_shared::VideoStatus;
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::db;
use crate::error::AppError;
use crate::state::{AppState, CAPTIONS_FILE, MP4_FILE, STORYBOARD_FILE, StoredVideo};

#[derive(Deserialize)]
pub struct MediaQuery {
    exp: Option<String>,
    sig: Option<String>,
}

fn content_type(file: &str) -> &'static str {
    match file {
        MP4_FILE => "video/mp4",
        STORYBOARD_FILE => "image/png",
        _ => "application/x-subrip; charset=utf-8",
    }
}

pub async fn serve(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
    Query(q): Query<MediaQuery>,
    req: Request,
) -> Response {
    match serve_inner(&state, &id, &file, q, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

async fn serve_inner(
    state: &AppState,
    id: &str,
    file: &str,
    q: MediaQuery,
    req: Request,
) -> Result<Response, AppError> {
    let not_found = || AppError::not_found("file");
    if uuid::Uuid::parse_str(id).is_err()
        || ![MP4_FILE, STORYBOARD_FILE, CAPTIONS_FILE].contains(&file)
    {
        return Err(not_found());
    }
    let exp = q.exp.as_deref().and_then(|e| e.parse::<i64>().ok());
    let (Some(exp), Some(sig)) = (exp, q.sig.as_deref()) else {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "bad_signature",
            "This media link isn't signed.",
        ));
    };
    if exp < chrono::Utc::now().timestamp() {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "link_expired",
            "This media link has expired. Reload the video.",
        ));
    }
    if !state.verify_media(id, file, exp, sig) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "bad_signature",
            "This media link isn't valid.",
        ));
    }
    let video: StoredVideo = state
        .store
        .get(db::VIDEO, id)
        .await?
        .ok_or_else(not_found)?;
    if video.video.status != VideoStatus::Ready || (file == CAPTIONS_FILE && !video.has_captions) {
        return Err(not_found());
    }
    let path = state.video_dir(id).join(file);
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(not_found());
    }
    let res = ServeFile::new(&path)
        .oneshot(req)
        .await
        .map_err(AppError::internal)?;
    let mut res = res.map(Body::new);
    if res.status().is_success() {
        res.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static(content_type(file)),
        );
        res.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=3600"),
        );
    }
    Ok(res)
}
