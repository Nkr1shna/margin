//! `/media/videos/{id}/{file}?exp&sig` and `/media/sources/{id}/{file}?exp&sig`: signed,
//! range-capable video files and source files.

use axum::body::Body;
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use margin_shared::{Source, VideoStatus};
use serde::Deserialize;
use tower::ServiceExt;
use tower_http::services::ServeFile;

use crate::db;
use crate::error::AppError;
use crate::sources;
use crate::state::{
    AppState, CAPTIONS_FILE, MP4_FILE, POSTER_FILE, SOURCE_ORIGINAL, STORYBOARD_FILE, StoredVideo,
};

#[derive(Deserialize)]
pub struct MediaQuery {
    exp: Option<String>,
    sig: Option<String>,
}

fn content_type(file: &str) -> &'static str {
    match file {
        MP4_FILE => "video/mp4",
        STORYBOARD_FILE | POSTER_FILE => "image/png",
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
        || ![MP4_FILE, STORYBOARD_FILE, CAPTIONS_FILE, POSTER_FILE].contains(&file)
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
    if video.video.status != VideoStatus::Ready
        || (file == CAPTIONS_FILE && !video.has_captions)
        || (file == POSTER_FILE && !video.has_poster)
    {
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

// ---------------------------------------------------------------------------
// Sources
// ---------------------------------------------------------------------------

/// `/media/sources/{id}/{file}?exp&sig`: a source's original file ("original"), a
/// rendered PDF page ("page-{n}.png") or a video frame ("frame-{ms}.jpg"), signed like
/// video media, range-capable.
pub async fn serve_source(
    State(state): State<AppState>,
    Path((id, file)): Path<(String, String)>,
    Query(q): Query<MediaQuery>,
    req: Request,
) -> Response {
    match serve_source_inner(&state, &id, &file, q, req).await {
        Ok(r) => r,
        Err(e) => e.into_response(),
    }
}

/// The media files a source may have besides its original: `page-{n}.png` (n ≥ 1) and
/// `frame-{ms}.jpg`, digits only.
fn is_source_media_file(file: &str) -> bool {
    let number = |s: &str| !s.is_empty() && s.len() <= 12 && s.bytes().all(|b| b.is_ascii_digit());
    file.strip_prefix("page-")
        .and_then(|r| r.strip_suffix(".png"))
        .is_some_and(|n| number(n) && !n.trim_start_matches('0').is_empty())
        || file
            .strip_prefix("frame-")
            .and_then(|r| r.strip_suffix(".jpg"))
            .is_some_and(number)
}

/// The content type of a source's original file, from its name. Text and code (HTML
/// included) are served as plain text, so nothing a learner uploads runs as a page.
pub fn original_content_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "webp" => "image/webp",
        "gif" => "image/gif",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        "flac" => "audio/flac",
        "caf" => "audio/x-caf",
        "mp4" => "video/mp4",
        "mov" => "video/quicktime",
        "m4v" => "video/x-m4v",
        _ => "text/plain; charset=utf-8",
    }
}

async fn serve_source_inner(
    state: &AppState,
    id: &str,
    file: &str,
    q: MediaQuery,
    req: Request,
) -> Result<Response, AppError> {
    let not_found = || AppError::not_found("file");
    if uuid::Uuid::parse_str(id).is_err()
        || (file != SOURCE_ORIGINAL && !is_source_media_file(file))
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
            "This media link has expired. Reload the library.",
        ));
    }
    if !state.verify_source_media(id, file, exp, sig) {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "bad_signature",
            "This media link isn't valid.",
        ));
    }
    let source: Source = state
        .store
        .get(db::SOURCE, id)
        .await?
        .ok_or_else(not_found)?;
    let (path, content_type) = if file == SOURCE_ORIGINAL {
        (
            state.source_dir(id).join(sources::FILE_NAME),
            original_content_type(&source.name),
        )
    } else if file.ends_with(".png") {
        (state.source_media_dir(id).join(file), "image/png")
    } else {
        (state.source_media_dir(id).join(file), "image/jpeg")
    };
    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
        return Err(not_found());
    }
    let res = ServeFile::new(&path)
        .oneshot(req)
        .await
        .map_err(AppError::internal)?;
    let mut res = res.map(Body::new);
    if res.status().is_success() {
        let headers = res.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        headers.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=3600"),
        );
        headers.insert(
            header::X_CONTENT_TYPE_OPTIONS,
            HeaderValue::from_static("nosniff"),
        );
        if file == SOURCE_ORIGINAL {
            let name = source.name.rsplit(['/', '\\']).next().unwrap_or("file");
            if let Ok(value) =
                HeaderValue::from_str(&format!("inline; filename*=UTF-8''{}", percent(name)))
            {
                headers.insert(header::CONTENT_DISPOSITION, value);
            }
        }
    }
    Ok(res)
}

/// RFC 5987 percent-encoding for a file name.
fn percent(name: &str) -> String {
    name.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_page_and_frame_names_are_served() {
        for ok in [
            "page-1.png",
            "page-120.png",
            "frame-0.jpg",
            "frame-2500.jpg",
        ] {
            assert!(is_source_media_file(ok), "{ok}");
        }
        for bad in [
            "page-0.png",
            "page-.png",
            "page-1.jpg",
            "frame-1.png",
            "frame--1.jpg",
            "../file",
            "page-1.png/..",
            "file",
            "original.png",
            "frame-12a.jpg",
        ] {
            assert!(!is_source_media_file(bad), "{bad}");
        }
        assert_eq!(original_content_type("Lecture.MOV"), "video/quicktime");
        assert_eq!(
            original_content_type("page.html"),
            "text/plain; charset=utf-8"
        );
        assert_eq!(original_content_type("README"), "text/plain; charset=utf-8");
        assert_eq!(percent("ch 1 é.pdf"), "ch%201%20%C3%A9.pdf");
    }
}
