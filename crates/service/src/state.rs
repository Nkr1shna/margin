//! Shared service state, the stored row shapes, and helpers every handler uses.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, KeyInit, Mac};
use margin_ai::{Credential, ProviderConfig};
use margin_embed::{Embedder, ModelFiles};
use margin_shared::pipeline::PipelineConfig;
use margin_shared::pipeline::Route;
use margin_shared::{
    Citation, Event, Id, Lesson, Note, Proposal, Provider, ReasoningEffort, RouteModels, Settings,
    Source, Topic, Video, VideoMedia, VideoStatus, error_code,
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
use crate::engines::{Generator, ModelChoice, Renderer, SourceTools};
use crate::error::{AppError, AppResult};
use crate::ingest::IngestQueue;
use crate::pipeline::{Pipeline, Registry};
use crate::principal::Principal;
use crate::retrieval::RetrievalCache;
use crate::secrets::{ANTHROPIC_API_KEY, OPENAI_API_KEY, SecretStore};
use crate::trace::Tracer;
use crate::turns::Turns;
use arc_swap::ArcSwap;

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
    /// What the lesson writer is told the learner asked for, for a lesson with no
    /// proposal (the agent's `write_lesson`); a lesson from a proposal reads its request.
    #[serde(default)]
    pub prompt: Option<String>,
    /// Whether a lesson with no proposal ends with a "Check yourself" quiz.
    #[serde(default)]
    pub with_quiz: bool,
    /// The attached folders the lesson draws on (its proposal's, or the chat's folders
    /// when the agent wrote it with `write_lesson`); retrieval covers those that still
    /// exist, every time it is written.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub workspace_ids: Vec<Id>,
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
    /// The person the device signed in as; `None` only before `people-v1`.
    #[serde(default)]
    pub person_id: Option<Id>,
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
    /// The retrieval and generation pipeline (`AppState.pipeline` is built from it).
    #[serde(default)]
    pub pipeline: PipelineConfig,
    #[serde(default = "trace_retention_days")]
    pub trace_retention_days: u32,
    /// One-time data migrations already done, by name (e.g. "passages-v1"), so each
    /// runs once.
    #[serde(default)]
    pub migrations: Vec<String>,
    /// The provider new chats and the job routes use (D9).
    #[serde(default)]
    pub default_provider: Provider,
    /// The OpenAI slots once stored; `None` (settings from before the slots) derives
    /// them from `openai_model` and the deprecated `agent_model`
    /// (`RouteModels::openai_from`).
    #[serde(default)]
    pub openai_models: Option<RouteModels>,
    /// The ChatGPT slots; `None` = `RouteModels::chatgpt_default`.
    #[serde(default)]
    pub chatgpt_models: Option<RouteModels>,
    /// The Claude slots; `None` = `RouteModels::claude_default`.
    #[serde(default)]
    pub claude_models: Option<RouteModels>,
}

fn trace_retention_days() -> u32 {
    margin_shared::TRACE_RETENTION_DAYS
}

impl SettingsRow {
    /// `provider`'s three slots as stored (before any pipeline override).
    pub fn slots(&self, provider: Provider) -> RouteModels {
        match provider {
            Provider::OpenAi => self.openai_models.clone().unwrap_or_else(|| {
                RouteModels::openai_from(self.openai_model.trim(), self.agent_model.as_deref())
            }),
            Provider::ChatGpt => self
                .chatgpt_models
                .clone()
                .unwrap_or_else(RouteModels::chatgpt_default),
            Provider::Claude => self
                .claude_models
                .clone()
                .unwrap_or_else(RouteModels::claude_default),
        }
    }

    /// The legacy `openai_model` patch: the writing slot; the chat slot too while it
    /// follows the writing one (no agent model of its own).
    pub fn set_openai_model(&mut self, model: &str) {
        let mut slots = self.slots(Provider::OpenAi);
        if slots.chat == slots.write || slots.chat.is_empty() {
            slots.chat = model.to_string();
        }
        slots.write = model.to_string();
        self.openai_model = model.to_string();
        self.openai_models = Some(slots);
    }

    /// The legacy `agent_model` patch: the chat slot, or back to the writing slot.
    pub fn set_agent_model(&mut self, model: Option<String>) {
        let mut slots = self.slots(Provider::OpenAi);
        slots.chat = model.clone().unwrap_or_else(|| slots.write.clone());
        self.agent_model = model;
        self.openai_models = Some(slots);
    }

    /// Replaces `provider`'s slots.
    pub fn set_slots(&mut self, provider: Provider, slots: RouteModels) {
        match provider {
            Provider::OpenAi => {
                self.openai_model = slots.write.clone();
                self.openai_models = Some(slots);
            }
            Provider::ChatGpt => self.chatgpt_models = Some(slots),
            Provider::Claude => self.claude_models = Some(slots),
        }
    }
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
            pipeline: PipelineConfig::default(),
            trace_retention_days: margin_shared::TRACE_RETENTION_DAYS,
            migrations: Vec::new(),
            default_provider: Provider::default(),
            openai_models: None,
            chatgpt_models: None,
            claude_models: None,
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
    /// Events with their audience; each socket sends what its principal may see
    /// (`Audience::allows`).
    pub events: broadcast::Sender<Emitted>,
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
    /// The speech model's download in progress or the last one's failure
    /// (`SourceTools::speech_model`).
    pub speech_job: Mutex<ModelJob>,
    /// Bytes the speech model's download fetches, once Hugging Face has told us.
    pub speech_download_bytes: Mutex<Option<u64>>,
    /// The re-index with a new embedder (`ingest::reembed`) while it runs.
    pub reindex: Mutex<Option<margin_shared::ReindexProgress>>,
    /// The last `EmbeddingModel` sent, so a progress event changes only its own part.
    pub last_model: Mutex<Option<margin_shared::EmbeddingModel>>,
    /// Sources to read, one at a time.
    pub source_queue: mpsc::UnboundedSender<Id>,
    /// Uploads in parts a request is working on (`uploads`), one request at a time each.
    pub uploads_busy: Mutex<HashSet<Id>>,
    /// Chat turns in progress.
    pub turns: Turns,
    /// Which devices are online, and tool requests sent to them.
    pub devices: Arc<DeviceHub>,
    /// Stage constructors by name (`pipeline::Registry::standard`).
    pub registry: Registry,
    /// The pipeline `Settings.pipeline` names, rebuilt when the settings change.
    pub pipeline: ArcSwap<Pipeline>,
    /// Trace rows on their way to the store.
    pub tracer: Tracer,
    /// Documents waiting to be indexed.
    pub ingest_queue: IngestQueue,
    pub retrieval_cache: RetrievalCache,
    /// When each folder's `WorkspaceChanged` last went out (`folders::emit_changed`).
    pub folder_sync_emits: Mutex<HashMap<Id, crate::folders::EmitSlot>>,
    /// Folder sources replaced by a new upload while being read: the read in progress
    /// leaves their status alone (the new copy is queued behind it).
    pub sources_replaced: Mutex<HashSet<Id>>,
    /// The model providers' endpoints, the ChatGPT sign-in and the model windows.
    pub providers: crate::providers::Providers,
    /// Each person's expanded read set, cleared whole after any person, group,
    /// membership or agent write (`groups::Expansions::invalidate_all`).
    pub expansions: crate::groups::Expansions,
    /// Where groups and memberships come from: `access::Manual` in L1 (L6 adds GitHub
    /// organisations).
    pub access_sources: Vec<Arc<dyn crate::access::AccessSource>>,
}

/// Who may receive an event (`docs/l1-brief.md` D8). The wire `Event` carries none of
/// this; `events::serve` filters on it.
#[derive(Debug, Clone, PartialEq)]
pub enum Audience {
    /// Everyone signed in: `Hello`, `SettingsChanged`, `EmbeddingModelChanged`,
    /// `PersonChanged`, `GroupChanged`, `GroupDeleted`, `MembershipChanged` (a directory,
    /// no content).
    Everyone,
    /// Organisation admins (and the owner).
    Admins,
    /// One person (by person id), on every device.
    Person(Id),
    /// One person, and the admins.
    PersonOrAdmins(Id),
    /// Whoever's expanded read set meets these paths (a row's `access`).
    Paths(Vec<String>),
}

impl Audience {
    pub fn allows(&self, p: &Principal) -> bool {
        match self {
            Audience::Everyone => true,
            Audience::Admins => p.org_role.is_admin(),
            Audience::Person(id) => &p.person_id == id,
            Audience::PersonOrAdmins(id) => &p.person_id == id || p.org_role.is_admin(),
            Audience::Paths(paths) => paths.iter().any(|path| p.paths.contains(path)),
        }
    }
}

/// An event on its way to the sockets.
#[derive(Debug, Clone)]
pub struct Emitted {
    pub audience: Audience,
    pub event: Event,
}

impl App {
    /// An event for `audience`.
    pub fn emit_to(&self, audience: Audience, event: Event) {
        // No receivers is fine: nobody is connected.
        let _ = self.events.send(Emitted { audience, event });
    }

    /// An event for everyone. Until L1-events names every emit site's audience this
    /// forwards everything as `Everyone` (today's behaviour); then it is only for the
    /// directory events `Audience::Everyone` lists, with a `debug_assert!`.
    pub fn emit(&self, event: Event) {
        self.emit_to(Audience::Everyone, event);
    }

    /// A row's event, to whoever may read the row (`access` must not be empty).
    pub fn emit_row(&self, access: &[String], event: Event) {
        debug_assert!(!access.is_empty(), "a row's access is never empty");
        self.emit_to(Audience::Paths(access.to_vec()), event);
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
        let has_claude_api_key = self
            .secret_get(ANTHROPIC_API_KEY)
            .await?
            .is_some_and(|k| !k.is_empty());
        let chatgpt = crate::providers::chatgpt_account(self).await?;
        Ok(Settings {
            openai_models: row.slots(Provider::OpenAi),
            chatgpt_models: row.slots(Provider::ChatGpt),
            claude_models: row.slots(Provider::Claude),
            openai_base_url: row.openai_base_url,
            openai_model: row.openai_model,
            has_api_key,
            voice: row.voice,
            render_width: row.render_width,
            render_height: row.render_height,
            render_fps: row.render_fps,
            reasoning_effort: row.reasoning_effort,
            agent_model: row.agent_model,
            pipeline: row.pipeline.with_embedder(self.model_files.backend.name()),
            trace_retention_days: row.trace_retention_days,
            default_provider: row.default_provider,
            has_claude_api_key,
            chatgpt,
        })
    }

    /// Where and how to call `provider` (D10): its base URL, credential and slots, each
    /// slot after the `pipeline.routes` override when that names one of this provider's
    /// models (an override for another provider is logged and ignored). `no_api_key`
    /// naming the provider when it has no key, `auth_failed` when ChatGPT is signed out
    /// (or its sign-in ran out). An empty slot is left empty here; `model_for` refuses
    /// it with `no_model`. The ChatGPT token is refreshed first when it is about to run
    /// out.
    pub async fn provider_config(&self, provider: Provider) -> AppResult<ProviderConfig> {
        let row = self.settings_row().await?;
        let endpoints = self.providers.endpoints();
        let key = |name| async move {
            Ok::<_, AppError>(
                self.secret_get(name)
                    .await?
                    .filter(|k| !k.trim().is_empty()),
            )
        };
        let (base_url, credential) = match provider {
            Provider::OpenAi => {
                let Some(api_key) = key(OPENAI_API_KEY).await? else {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        error_code::NO_API_KEY,
                        "Add an OpenAI API key in Settings → AI first. It's kept in the service's secret store and never sent back.",
                    ));
                };
                (row.openai_base_url.clone(), Credential::ApiKey(api_key))
            }
            Provider::Claude => {
                let Some(api_key) = key(ANTHROPIC_API_KEY).await? else {
                    return Err(AppError::new(
                        StatusCode::BAD_REQUEST,
                        error_code::NO_API_KEY,
                        "Add a Claude API key in Settings → AI first. It's kept in the service's secret store and never sent back.",
                    ));
                };
                (endpoints.anthropic.clone(), Credential::ApiKey(api_key))
            }
            Provider::ChatGpt => {
                let token = crate::providers::chatgpt_access_token(self, None).await?;
                (endpoints.chatgpt_api.clone(), Credential::Bearer(token))
            }
        };
        let routes = self.pipeline.load().routes.clone();
        let mut models = row.slots(provider);
        for route in Route::ALL {
            let Some(m) = routes.get(route).map(str::trim).filter(|m| !m.is_empty()) else {
                continue;
            };
            if crate::providers::override_applies(self, provider, m) {
                models.set(route, m);
            } else {
                tracing::info!(
                    %provider, ?route, model = %m,
                    "the pipeline's route override isn't one of this provider's models; ignored"
                );
            }
        }
        Ok(ProviderConfig {
            provider,
            base_url,
            credential,
            models,
        })
    }

    /// The provider, model and window a call on `route` uses: `provider`'s config
    /// (Settings' default provider when `None`) and its slot for the route. `no_api_key`
    /// or `auth_failed` without a credential, `no_model` when the slot is empty. The
    /// window comes from the provider's model list or `margin_ai::windows`, else
    /// `MODEL_WINDOW_FALLBACK` (logged).
    pub async fn model_for(
        &self,
        route: Route,
        provider: Option<Provider>,
    ) -> AppResult<ModelChoice> {
        let provider = match provider {
            Some(p) => p,
            None => self.settings_row().await?.default_provider,
        };
        let config = self.provider_config(provider).await?;
        let model = config.models.get(route).trim().to_string();
        if model.is_empty() {
            let slot = match route {
                Route::Chat => "chats",
                Route::Write => "writing",
                Route::Small => "small tasks",
            };
            return Err(AppError::new(
                StatusCode::BAD_REQUEST,
                error_code::NO_MODEL,
                format!(
                    "Choose a model for {slot} in Settings → AI → {} first.",
                    provider.label()
                ),
            ));
        }
        let window = crate::providers::window(self, &config, &model).await;
        Ok(ModelChoice {
            config,
            model,
            window,
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
