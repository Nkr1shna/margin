//! Shared service state, the stored row shapes, and helpers every handler uses.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use margin_embed::{Embedder, ModelFiles};
use margin_shared::{
    Citation, Event, Id, Lesson, Note, Proposal, ReasoningEffort, Settings, Source, Topic, Video,
    VideoMedia, VideoStatus,
};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::task::AbortHandle;

use crate::auth::{Login, RateLimiter};
use crate::config::Config;
use crate::db::{self, Store};
use crate::devices::DeviceHub;
use crate::embedding::ModelJob;
use crate::engines::{AiConfig, Generator, Renderer, SourceTools};
use crate::error::{AppError, AppResult};
use crate::secrets::{OPENAI_API_KEY, SecretStore};
use crate::turns::Turns;

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
/// A source's uploaded file, by the name the source media route serves it under.
pub const SOURCE_ORIGINAL: &str = "original";
/// The folder inside a source's folder for its rendered pages and video frames.
pub const SOURCE_MEDIA_DIR: &str = "media";

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
    /// The citations that went with `previous_body`.
    #[serde(default)]
    pub previous_citations: Option<Vec<Citation>>,
    /// Who wrote the body as it is now (owner decision Q2): `Agent` once a lesson job
    /// writes it, `Owner` once it is edited through `PATCH /api/lessons/{id}`. Rows
    /// written before this field existed read as the owner's.
    #[serde(default)]
    pub author: Author,
}

/// Who wrote a note's or a lesson's text, for owner decision Q2: the agent changes its
/// own writing freely but asks before overwriting the owner's.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Author {
    /// The owner's own writing; also what a row without the field reads as (the safe
    /// default: overwriting it asks first).
    #[default]
    Owner,
    /// Written by the agent (a tool) or a generation job.
    Agent,
}

/// A note as stored in table `note`: the wire `Note` plus who wrote it. Reading a row as
/// a plain `Note` ignores `author`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredNote {
    #[serde(flatten)]
    pub note: Note,
    /// `Agent` for a note an agent tool created (and whose body it replaced), `Owner`
    /// for one created through `POST /api/notes` or whose body was changed through
    /// `PATCH /api/notes/{id}`. Appending never changes it.
    #[serde(default)]
    pub author: Author,
}

// Stored lessons keep each citation's `image_url` as the bare name of the source's media
// file ("page-3.png"); `App::public_lesson` turns it into a signed URL on the way out.

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
    #[serde(default)]
    pub reasoning_effort: ReasoningEffort,
    /// The model chats use; `None` uses `openai_model`.
    #[serde(default)]
    pub agent_model: Option<String>,
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
            reasoning_effort: ReasoningEffort::default(),
            agent_model: None,
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
    pub source_tools: Arc<dyn SourceTools>,
    /// Hugging Face base URL the embedding model is downloaded from.
    pub hf_base: String,
    /// For model downloads: no overall time limit, only for stalls.
    pub download_http: reqwest::Client,
    pub model_files: ModelFiles,
    /// The embedding model, loaded on first use once downloaded.
    pub embedder: tokio::sync::Mutex<Option<Arc<dyn Embedder>>>,
    /// A download in progress or the last one's failure (on-disk state is checked live).
    pub model_job: Mutex<ModelJob>,
    /// Bytes a download fetches, once Hugging Face has told us.
    pub model_download_bytes: Mutex<Option<u64>>,
    /// Sources to read, one at a time.
    pub source_queue: mpsc::UnboundedSender<Id>,
    /// Uploads in parts a request is working on (`uploads`), one request at a time each.
    pub uploads_busy: Mutex<HashSet<Id>>,
    /// Chat turns in progress.
    pub turns: Turns,
    /// Which devices are online, and tool requests sent to them.
    pub devices: Arc<DeviceHub>,
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
            lesson: self.public_lesson(lesson),
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

    pub fn emit_source(&self, source: &Source) {
        self.emit(Event::SourceChanged {
            source: self.public_source(source),
        });
    }

    pub fn video_dir(&self, video_id: &str) -> PathBuf {
        self.config.videos_dir().join(video_id)
    }

    /// The folder holding an uploaded source's `file`.
    pub fn source_dir(&self, source_id: &str) -> PathBuf {
        self.config.sources_dir().join(source_id)
    }

    /// Where a source's rendered pages and video frames are kept.
    pub fn source_media_dir(&self, source_id: &str) -> PathBuf {
        self.source_dir(source_id).join(SOURCE_MEDIA_DIR)
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

    /// What a source media URL signs: `source/{id}/{file}/{exp}`, kept apart from video
    /// signatures by the prefix.
    fn source_mac(&self, source_id: &str, file: &str, exp: i64) -> Hmac<Sha256> {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(&self.media_key)
            .expect("HMAC accepts any key length");
        mac.update(format!("source/{source_id}/{file}/{exp}").as_bytes());
        mac
    }

    pub fn source_media_signature(&self, source_id: &str, file: &str, exp: i64) -> String {
        hex::encode(
            self.source_mac(source_id, file, exp)
                .finalize()
                .into_bytes(),
        )
    }

    /// Checks a source media signature in constant time and that `exp` hasn't passed.
    pub fn verify_source_media(&self, source_id: &str, file: &str, exp: i64, sig: &str) -> bool {
        if exp < Utc::now().timestamp() {
            return false;
        }
        let Ok(sig) = hex::decode(sig) else {
            return false;
        };
        self.source_mac(source_id, file, exp)
            .verify_slice(&sig)
            .is_ok()
    }

    /// `/media/sources/{id}/{file}?exp&sig`, valid for `MEDIA_URL_TTL_SECONDS`.
    pub fn source_media_url(&self, source_id: &str, file: &str) -> String {
        let exp = (Utc::now() + Duration::seconds(MEDIA_URL_TTL_SECONDS)).timestamp();
        let sig = self.source_media_signature(source_id, file, exp);
        format!(
            "{}/media/sources/{source_id}/{file}?exp={exp}&sig={sig}",
            self.config.public_url
        )
    }

    /// The wire `Source`, with a signed URL of the original file.
    pub fn public_source(&self, source: &Source) -> Source {
        let mut source = source.clone();
        source.media_url = Some(self.source_media_url(&source.id, SOURCE_ORIGINAL));
        source
    }

    /// The wire `Lesson`: citation pictures (stored as media file names) become signed
    /// URLs.
    pub fn public_lesson(&self, lesson: &Lesson) -> Lesson {
        let mut lesson = lesson.clone();
        for c in &mut lesson.citations {
            c.image_url = c
                .image_url
                .take()
                .map(|file| self.source_media_url(&c.source_id, &file));
        }
        lesson
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
            reasoning_effort: row.reasoning_effort,
            agent_model: row.agent_model,
        })
    }

    /// The Responses API configuration, or the `no_api_key` / `no_model` error to show.
    pub async fn ai_config(&self) -> AppResult<AiConfig> {
        let row = self.settings_row().await?;
        let model = row.openai_model.clone();
        self.ai_config_with(row, model).await
    }

    /// The configuration chat turns use: its model is `agent_model` when set, else
    /// `openai_model`; `no_model` only when neither is.
    pub async fn agent_ai_config(&self) -> AppResult<AiConfig> {
        let row = self.settings_row().await?;
        let model = row
            .agent_model
            .clone()
            .filter(|m| !m.trim().is_empty())
            .unwrap_or_else(|| row.openai_model.clone());
        self.ai_config_with(row, model).await
    }

    async fn ai_config_with(&self, row: SettingsRow, model: String) -> AppResult<AiConfig> {
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
        if model.trim().is_empty() {
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                "no_model",
                "Choose a model in Settings → AI provider first: enter the name of a model your API key can use with the Responses API.",
            ));
        }
        Ok(AiConfig {
            base_url: row.openai_base_url,
            api_key,
            model: model.trim().to_string(),
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
