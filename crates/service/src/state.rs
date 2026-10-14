//! Shared service state, the stored row shapes, and helpers every handler uses.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use margin_shared::{Event, Id, Lesson, Proposal, Settings, Topic, Video, VideoMedia, VideoStatus};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::AbortHandle;

use crate::auth::{Login, RateLimiter};
use crate::config::Config;
use crate::db::{self, Store};
use crate::engines::{AiConfig, Generator, Renderer};
use crate::error::{AppError, AppResult};
use crate::secrets::{OPENAI_API_KEY, SecretStore};

pub type AppState = Arc<App>;

pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_RENDER_WIDTH: u32 = 1920;
pub const DEFAULT_RENDER_HEIGHT: u32 = 1080;
pub const DEFAULT_RENDER_FPS: u32 = 30;
/// How long signed media URLs stay valid.
pub const MEDIA_URL_TTL_SECONDS: i64 = 6 * 60 * 60;
/// Max concurrent lesson generations.
pub const LESSON_CONCURRENCY: usize = 3;
pub const INTERRUPTED: &str = "Interrupted when the service stopped.";

/// Media files a video folder may serve, by the names the media route accepts.
pub const MP4_FILE: &str = "video.mp4";
pub const STORYBOARD_FILE: &str = "storyboard.png";
pub const CAPTIONS_FILE: &str = "captions.srt";
pub const POSTER_FILE: &str = margin_video::POSTER_FILE;

// ---------------------------------------------------------------------------
// Stored rows (wire types plus service-only fields)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredLesson {
    #[serde(flatten)]
    pub lesson: Lesson,
    /// The proposal this lesson was created from (for prompt, quiz and video options).
    #[serde(default)]
    pub proposal_id: Option<Id>,
    /// The body before a regeneration started, restored if it fails or is interrupted.
    #[serde(default)]
    pub previous_body: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredVideo {
    #[serde(flatten)]
    pub video: Video,
    #[serde(default)]
    pub has_captions: bool,
    /// Whether the last successful render left a `poster.png`. Absent (false) on rows
    /// written before posters existed.
    #[serde(default)]
    pub has_poster: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceRow {
    pub id: Id,
    pub name: String,
    /// Hex SHA-256 of the device token.
    pub token_hash: String,
    pub github_login: String,
    pub created_at: DateTime<Utc>,
    pub last_seen_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SettingsRow {
    pub openai_base_url: String,
    /// Empty until the owner picks one in Settings.
    pub openai_model: String,
    pub voice: Option<String>,
    pub render_width: u32,
    pub render_height: u32,
    pub render_fps: u32,
}

impl Default for SettingsRow {
    fn default() -> Self {
        Self {
            openai_base_url: DEFAULT_OPENAI_BASE_URL.into(),
            openai_model: String::new(),
            voice: None,
            render_width: DEFAULT_RENDER_WIDTH,
            render_height: DEFAULT_RENDER_HEIGHT,
            render_fps: DEFAULT_RENDER_FPS,
        }
    }
}

pub const SETTINGS_ID: &str = "main";

// ---------------------------------------------------------------------------
// App
// ---------------------------------------------------------------------------

pub struct App {
    pub config: Config,
    pub store: Store,
    pub secrets: Arc<dyn SecretStore>,
    pub generator: Arc<dyn Generator>,
    pub renderer: Arc<dyn Renderer>,
    pub allow_unsandboxed_render: bool,
    pub http: reqwest::Client,
    pub media_key: Vec<u8>,
    pub events: broadcast::Sender<Event>,
    /// Device ids revoked; open sockets for them close.
    pub revocations: broadcast::Sender<Id>,
    pub shutdown: watch::Receiver<bool>,
    /// Serializes read-modify-write sequences on the store.
    pub write_lock: tokio::sync::Mutex<()>,
    pub logins: Mutex<HashMap<Id, Login>>,
    pub limiter: RateLimiter,
    pub lesson_slots: Arc<Semaphore>,
    pub lesson_jobs: Mutex<HashMap<Id, AbortHandle>>,
    pub video_queue: mpsc::UnboundedSender<Id>,
    /// The video currently rendering, so deleting it can stop the render.
    pub render_job: Mutex<Option<(Id, AbortHandle)>>,
}

impl App {
    pub fn emit(&self, event: Event) {
        // No receivers is fine: nobody is connected.
        let _ = self.events.send(event);
    }

    pub fn emit_topic(&self, topic: &Topic) {
        self.emit(Event::TopicChanged {
            topic: topic.clone(),
        });
    }

    pub fn emit_lesson(&self, lesson: &Lesson) {
        self.emit(Event::LessonChanged {
            lesson: lesson.clone(),
        });
    }

    pub fn emit_proposal(&self, proposal: &Proposal) {
        self.emit(Event::ProposalChanged {
            proposal: proposal.clone(),
        });
    }

    pub fn emit_video(&self, video: &StoredVideo) {
        self.emit(Event::VideoChanged {
            video: self.public_video(video),
        });
    }

    pub fn video_dir(&self, video_id: &str) -> PathBuf {
        self.config.videos_dir().join(video_id)
    }

    // -- media signing -----------------------------------------------------

    pub fn media_signature(&self, video_id: &str, file: &str, exp: i64) -> String {
        hex::encode(self.media_mac(video_id, file, exp).finalize().into_bytes())
    }

    fn media_mac(&self, video_id: &str, file: &str, exp: i64) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.media_key)
            .expect("HMAC accepts any key length");
        mac.update(format!("{video_id}/{file}/{exp}").as_bytes());
        mac
    }

    /// Checks `sig` in constant time and that `exp` hasn't passed.
    pub fn verify_media(&self, video_id: &str, file: &str, exp: i64, sig: &str) -> bool {
        if exp < Utc::now().timestamp() {
            return false;
        }
        let Ok(sig) = hex::decode(sig) else {
            return false;
        };
        self.media_mac(video_id, file, exp)
            .verify_slice(&sig)
            .is_ok()
    }

    pub fn media_url(&self, video_id: &str, file: &str, exp: i64) -> String {
        let sig = self.media_signature(video_id, file, exp);
        format!(
            "{}/media/videos/{video_id}/{file}?exp={exp}&sig={sig}",
            self.config.public_url
        )
    }

    /// The wire `Video`, with signed media URLs when it is Ready.
    pub fn public_video(&self, stored: &StoredVideo) -> Video {
        let mut video = stored.video.clone();
        video.media = None;
        if video.status == VideoStatus::Ready {
            let expires_at = Utc::now() + Duration::seconds(MEDIA_URL_TTL_SECONDS);
            let exp = expires_at.timestamp();
            video.media = Some(VideoMedia {
                mp4_url: self.media_url(&video.id, MP4_FILE, exp),
                storyboard_url: self.media_url(&video.id, STORYBOARD_FILE, exp),
                captions_url: stored
                    .has_captions
                    .then(|| self.media_url(&video.id, CAPTIONS_FILE, exp)),
                poster_url: stored
                    .has_poster
                    .then(|| self.media_url(&video.id, POSTER_FILE, exp)),
                expires_at: DateTime::from_timestamp(exp, 0).unwrap_or(expires_at),
            });
        }
        video
    }

    // -- secrets -----------------------------------------------------------

    pub async fn secret_get(&self, name: &'static str) -> anyhow::Result<Option<String>> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || secrets.get(name)).await?
    }

    pub async fn secret_set(&self, name: &'static str, value: String) -> anyhow::Result<()> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || secrets.set(name, &value)).await?
    }

    pub async fn secret_delete(&self, name: &'static str) -> anyhow::Result<()> {
        let secrets = self.secrets.clone();
        tokio::task::spawn_blocking(move || secrets.delete(name)).await?
    }

    // -- settings ----------------------------------------------------------

    pub async fn settings_row(&self) -> anyhow::Result<SettingsRow> {
        Ok(self
            .store
            .get(db::SETTINGS, SETTINGS_ID)
            .await?
            .unwrap_or_default())
    }

    pub async fn settings(&self) -> anyhow::Result<Settings> {
        let row = self.settings_row().await?;
        let has_api_key = self
            .secret_get(OPENAI_API_KEY)
            .await?
            .is_some_and(|k| !k.is_empty());
        Ok(Settings {
            openai_base_url: row.openai_base_url,
            openai_model: row.openai_model,
            has_api_key,
            voice: row.voice,
            render_width: row.render_width,
            render_height: row.render_height,
            render_fps: row.render_fps,
        })
    }

    /// The Responses API configuration, or the `no_api_key` / `no_model` error to show.
    pub async fn ai_config(&self) -> AppResult<AiConfig> {
        let row = self.settings_row().await?;
        let key = self
            .secret_get(OPENAI_API_KEY)
            .await?
            .filter(|k| !k.is_empty());
        let Some(api_key) = key else {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "no_api_key",
                "Add an OpenAI API key in Settings → AI provider first. It's stored in the service Mac's Keychain and never sent back.",
            ));
        };
        if row.openai_model.trim().is_empty() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "no_model",
                "Choose a model in Settings → AI provider first: enter the name of a model your API key can use with the Responses API.",
            ));
        }
        Ok(AiConfig {
            base_url: row.openai_base_url,
            api_key,
            model: row.openai_model.trim().to_string(),
        })
    }
}

/// 32 random bytes, base64url without padding.
pub fn random_token() -> String {
    use base64::Engine;
    let mut bytes = [0u8; 32];
    rand::fill(&mut bytes[..]);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// Hex SHA-256, for storing tokens and poll secrets.
pub fn sha256_hex(text: &str) -> String {
    use sha2::Digest;
    hex::encode(Sha256::digest(text.as_bytes()))
}

pub fn new_id() -> Id {
    uuid::Uuid::new_v4().to_string()
}
