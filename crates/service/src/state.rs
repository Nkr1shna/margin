//! Shared service state, the stored row shapes, and helpers every handler uses.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU64;
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
    Source, Topic, Video, VideoMedia, VideoStatus, error_code, person_id,
};
use serde::de::DeserializeOwned;
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
use crate::principal::{BOOTSTRAP_PERSON_ID, Principal, PrincipalKind};
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
    /// What wrote the scene (R2, `docs/r2-brief.md` D4.3: the scene prompt's version, the
    /// pipeline, the provider and model); `None` until one is written. Service-only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub written_with: Option<margin_shared::WrittenWith>,
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
    /// The provider new chats and the job routes use (D9), once an admin picks one;
    /// `None` = the first provider with a credential (`Settings::first_configured`).
    #[serde(default)]
    pub default_provider: Option<Provider>,
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
    /// The `db::SCHEMA_VERSION` of the build that last opened this data. A row from
    /// before phase 5 has none (0) and reads as 1 (`schema_version()`). A newer one than
    /// the build's refuses to serve; an older one is backed up before migrating
    /// (`docs/phase5-brief.md` D5).
    #[serde(default)]
    pub schema_version: u32,
}

fn trace_retention_days() -> u32 {
    margin_shared::TRACE_RETENTION_DAYS
}

impl SettingsRow {
    /// `schema_version`, with a row from before the field read as version 1.
    pub fn schema_version(&self) -> u32 {
        self.schema_version.max(1)
    }

    /// `provider`'s three slots as stored (before any pipeline override), each slot
    /// never picked (missing, or stored empty) read as the provider's shipped default.
    pub fn slots(&self, provider: Provider) -> RouteModels {
        let stored = match provider {
            Provider::OpenAi => self.openai_models.clone().unwrap_or_else(|| {
                RouteModels::openai_from(self.openai_model.trim(), self.agent_model.as_deref())
            }),
            Provider::ChatGpt => self.chatgpt_models.clone().unwrap_or_default(),
            Provider::Claude => self.claude_models.clone().unwrap_or_default(),
        };
        stored.or_defaults(provider)
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
            default_provider: None,
            openai_models: None,
            chatgpt_models: None,
            claude_models: None,
            schema_version: db::SCHEMA_VERSION,
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
    /// The reranker models' downloads in progress or their last failures, and their
    /// sizes once Hugging Face has said (R2, `embedding::RerankerJobs`).
    pub reranker_jobs: Mutex<crate::embedding::RerankerJobs>,
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
    /// Alerts raised from the traces (`alerts::run`).
    pub alerts: Arc<Mutex<crate::alerts::Alerts>>,
    /// HTTP answers with a 5xx status since start (`server::log_requests`), for the
    /// alert job's `http_5xx` count.
    pub http_errors: AtomicU64,
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

impl Emitted {
    /// A row's event, to whoever may read the row (its effective access).
    pub fn row(row: &impl Owned, event: Event) -> Self {
        Self {
            audience: Audience::Paths(row.audience()),
            event,
        }
    }

    /// An event for whoever may read rows with `access`. An empty `access` reaches
    /// nobody (fails closed).
    pub fn paths(access: &[String], event: Event) -> Self {
        Self {
            audience: Audience::Paths(access.to_vec()),
            event,
        }
    }
}

/// Where `App::emit` sends an event that isn't a directory event (a mistake: logged).
/// Device and invitation events go to the admins, who may see them anyway; anything
/// else may carry someone's content (owner Q3: admins don't read it) and reaches
/// nobody.
pub fn fallback_audience(event: &Event) -> Audience {
    match event {
        Event::DeviceChanged { .. }
        | Event::DeviceDeleted { .. }
        | Event::InvitationChanged { .. }
        | Event::InvitationDeleted { .. }
        | Event::AgentChanged { .. }
        | Event::AgentDeleted { .. } => Audience::Admins,
        Event::AccessChanged { person_id } => Audience::Person(person_id.clone()),
        _ => Audience::Paths(Vec::new()),
    }
}

/// Whether `event` goes to everyone signed in through `App::emit`: the directory
/// events, which carry no content (D8). Every other event names its audience.
pub fn is_directory_event(event: &Event) -> bool {
    matches!(
        event,
        Event::Hello { .. }
            | Event::SettingsChanged { .. }
            | Event::EmbeddingModelChanged { .. }
            | Event::PersonChanged { .. }
            | Event::GroupChanged { .. }
            | Event::GroupDeleted { .. }
            | Event::MembershipChanged { .. }
    )
}

impl App {
    /// An event for `audience`.
    pub fn emit_to(&self, audience: Audience, event: Event) {
        // No receivers is fine: nobody is connected.
        let _ = self.events.send(Emitted { audience, event });
    }

    /// An event for everyone: only the directory events (`is_directory_event`). Any
    /// other event sent this way fails closed (`fallback_audience`) and the service
    /// logs the mistake, so an emit site that forgot its audience never reaches every
    /// member.
    pub fn emit(&self, event: Event) {
        if is_directory_event(&event) {
            self.emit_to(Audience::Everyone, event);
        } else {
            tracing::error!(
                ?event,
                "an event was emitted without an audience; sent to its fallback audience"
            );
            self.emit_to(fallback_audience(&event), event);
        }
    }

    /// A row's event, to whoever may read the row (`access` must not be empty; empty
    /// reaches nobody).
    pub fn emit_row(&self, access: &[String], event: Event) {
        debug_assert!(!access.is_empty(), "a row's access is never empty");
        let e = Emitted::paths(access, event);
        self.emit_to(e.audience, e.event);
    }

    /// `row`'s event, to whoever may read it (`Owned::audience`).
    pub fn emit_owned(&self, row: &impl Owned, event: Event) {
        self.emit_to(Audience::Paths(row.audience()), event);
    }

    /// Events collected under the write lock, each with its audience.
    pub fn emit_all(&self, events: impl IntoIterator<Item = Emitted>) {
        for e in events {
            self.emit_to(e.audience, e.event);
        }
    }

    pub fn emit_topic(&self, topic: &Topic) {
        self.emit_owned(
            topic,
            Event::TopicChanged {
                topic: topic.clone(),
            },
        );
    }

    pub fn emit_lesson(&self, lesson: &Lesson) {
        self.emit_owned(
            lesson,
            Event::LessonChanged {
                lesson: self.public_lesson(lesson),
            },
        );
    }

    pub fn emit_proposal(&self, proposal: &Proposal) {
        self.emit_owned(
            proposal,
            Event::ProposalChanged {
                proposal: proposal.clone(),
            },
        );
    }

    /// A video's event goes to whoever may read its lesson.
    pub fn emit_video(&self, video: &StoredVideo, lesson: &Lesson) {
        self.emit_owned(
            lesson,
            Event::VideoChanged {
                video: self.public_video(video),
            },
        );
    }

    pub fn emit_source(&self, source: &Source) {
        self.emit_owned(
            source,
            Event::SourceChanged {
                source: self.public_source(source),
            },
        );
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
        let openai_models = row.slots(Provider::OpenAi);
        let mut settings = Settings {
            // The legacy field mirrors the writing slot, its default included.
            openai_model: openai_models.write.clone(),
            openai_models,
            chatgpt_models: row.slots(Provider::ChatGpt),
            claude_models: row.slots(Provider::Claude),
            openai_base_url: row.openai_base_url,
            has_api_key,
            voice: row.voice,
            render_width: row.render_width,
            render_height: row.render_height,
            render_fps: row.render_fps,
            reasoning_effort: row.reasoning_effort,
            agent_model: row.agent_model,
            pipeline: row.pipeline.with_embedder(self.model_files.backend.name()),
            trace_retention_days: row.trace_retention_days,
            default_provider: Provider::default(),
            has_claude_api_key,
            chatgpt,
        };
        settings.default_provider = row
            .default_provider
            .unwrap_or_else(|| settings.first_configured());
        Ok(settings)
    }

    /// Settings' default provider: the one an admin picked, else the first provider with
    /// a credential (OpenAI when none has one).
    pub async fn default_provider(&self) -> anyhow::Result<Provider> {
        match self.settings_row().await?.default_provider {
            Some(p) => Ok(p),
            None => Ok(self.settings().await?.default_provider),
        }
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
            None => self.default_provider().await?,
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

// ---------------------------------------------------------------------------
// Ownership and access (L1, `docs/l1-brief.md` D6)
// ---------------------------------------------------------------------------

/// A library row with an owner and an audience: `topic`, `lesson`, `note`, `chat`,
/// `source`, `workspace`, `proposal`.
pub trait Owned {
    /// The person whose row it is; `None` only before `people-v1`.
    fn owner(&self) -> Option<&str>;
    /// Who may read it, as stored.
    fn access(&self) -> &[String];
    /// Who is refused even when `access` admits them (evaluated from L2).
    fn deny(&self) -> &[String];
    /// Who may read it: `access`, or, read empty, private to its owner (defence in
    /// depth: L1 never writes an empty one). A row from before `people-v1` (no owner) is
    /// the bootstrap owner's, the only person there can be then.
    fn audience(&self) -> Vec<String> {
        effective_access(self.owner(), self.access())
    }
    /// The owner, the bootstrap owner for a row from before `people-v1`.
    fn owner_id(&self) -> &str {
        self.owner().unwrap_or(BOOTSTRAP_PERSON_ID)
    }
}

macro_rules! owned {
    ($($t:ty),*) => {$(
        impl Owned for $t {
            fn owner(&self) -> Option<&str> {
                self.owner.as_deref()
            }
            fn access(&self) -> &[String] {
                &self.access
            }
            fn deny(&self) -> &[String] {
                &self.deny
            }
        }
    )*};
}
owned!(
    Topic,
    Lesson,
    Note,
    Proposal,
    Source,
    margin_shared::Chat,
    margin_shared::Workspace
);

impl Owned for StoredLesson {
    fn owner(&self) -> Option<&str> {
        self.lesson.owner()
    }
    fn access(&self) -> &[String] {
        self.lesson.access()
    }
    fn deny(&self) -> &[String] {
        self.lesson.deny()
    }
}

impl Owned for StoredNote {
    fn owner(&self) -> Option<&str> {
        self.note.owner()
    }
    fn access(&self) -> &[String] {
        self.note.access()
    }
    fn deny(&self) -> &[String] {
        self.note.deny()
    }
}

/// `access`, or `[person/<owner>]` when it is empty (see `Owned::audience`).
pub fn effective_access(owner: Option<&str>, access: &[String]) -> Vec<String> {
    if access.is_empty() {
        vec![person_id(owner.unwrap_or(BOOTSTRAP_PERSON_ID))]
    } else {
        access.to_vec()
    }
}

/// What a new row is stamped with (D6): its owner, who may read it, who is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ownership {
    pub owner: Id,
    pub access: Vec<String>,
    pub deny: Vec<String>,
}

impl Ownership {
    /// A new row of `p`'s, private to its person: an agent's rows belong to its owner.
    pub fn private(p: &Principal) -> Self {
        Self::private_to(&p.person_id)
    }

    /// A new row private to `person`.
    pub fn private_to(person: &str) -> Self {
        Self {
            owner: person.to_string(),
            access: vec![person_id(person)],
            deny: Vec::new(),
        }
    }

    /// A row filed under `parent` (a lesson, note or source under a topic, a source in
    /// a folder): the parent's owner, access and deny, so the parent is the unit of
    /// audience.
    pub fn of(parent: &(impl Owned + ?Sized)) -> Self {
        Self {
            owner: parent.owner_id().to_string(),
            access: parent.audience(),
            deny: parent.deny().to_vec(),
        }
    }
}

/// Whether `p` may read `row` (its audience meets `p`'s expanded paths). `deny` is
/// written and read back but not evaluated until L2.
pub fn can_read(p: &Principal, row: &(impl Owned + ?Sized)) -> bool {
    row.audience().iter().any(|a| p.paths.contains(a))
}

/// `row` when `p` may read it, else 404 `what` (never 403: a row outside the caller's
/// access doesn't exist for them).
pub fn readable<T: Owned>(p: &Principal, row: Option<T>, what: &str) -> AppResult<T> {
    match row {
        Some(r) if can_read(p, &r) => Ok(r),
        _ => Err(AppError::not_found(what)),
    }
}

/// 403 `not_owner` unless `p`'s person owns `row`. Admins are not exempt (owner Q3).
pub fn check_owner(p: &Principal, row: &(impl Owned + ?Sized)) -> AppResult<()> {
    if row.owner_id() == p.person_id {
        Ok(())
    } else {
        Err(not_owner())
    }
}

pub fn not_owner() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        margin_shared::access_code::NOT_OWNER,
        "Only its owner can change this.",
    )
}

/// The SurrealQL condition a list query adds for the principal (`access_binds`): rows
/// whose `access` meets `$paths` (the `<table>_access` indexes), and rows read with an
/// empty `access` that are the caller's own (`Owned::audience`). `can_read` is applied
/// to the result as well.
pub const ACCESS_WHERE: &str = "(doc.access CONTAINSANY $paths \
     OR (array::len(doc.access ?? []) = 0 AND (doc.owner ?? $bootstrap) = $person))";

/// The parameters `ACCESS_WHERE` reads.
pub fn access_binds(p: &Principal) -> Vec<(&'static str, serde_json::Value)> {
    vec![
        ("paths", serde_json::json!(p.paths)),
        ("person", serde_json::json!(p.person_id)),
        ("bootstrap", serde_json::json!(BOOTSTRAP_PERSON_ID)),
    ]
}

/// Every row of `table` `p` may read. `table` is one of `db`'s constants.
pub async fn list_visible<T: DeserializeOwned + Owned>(
    store: &Store,
    table: &'static str,
    p: &Principal,
) -> anyhow::Result<Vec<T>> {
    let sql = format!("SELECT VALUE doc FROM {table} WHERE {ACCESS_WHERE}");
    let mut rows: Vec<T> = store.query_rows(&sql, access_binds(p)).await?;
    rows.retain(|r| can_read(p, r));
    Ok(rows)
}

/// The principal of `person` acting for themself (a chat's owner while their turn runs,
/// a job writing their rows): their stored person and expansion. The bootstrap owner
/// (before `people-v1`) has no row.
pub async fn person_principal(state: &AppState, person: &str) -> AppResult<Arc<Principal>> {
    let row: Option<crate::people::PersonRow> = state.store.get(db::PERSON, person).await?;
    let person_row = match row {
        Some(r) => r.person,
        None if person == BOOTSTRAP_PERSON_ID => {
            let login = state.config.allowed_github_login.clone();
            margin_shared::Person {
                id: BOOTSTRAP_PERSON_ID.into(),
                handle: margin_shared::handle_for_login(&login),
                name: login.clone(),
                github_login: login,
                github_id: None,
                avatar_url: None,
                org_role: margin_shared::OrgRole::Owner,
                state: margin_shared::PersonState::Active,
                groups: Vec::new(),
                settings: margin_shared::PersonSettings::default(),
                created_at: Utc::now(),
            }
        }
        None => return Err(AppError::not_found("person")),
    };
    let expansion = crate::groups::expansion(state, person).await?;
    Ok(Arc::new(Principal {
        kind: PrincipalKind::Person,
        person_id: person.to_string(),
        id: person_id(person),
        paths: expansion.paths.clone(),
        deny_ids: expansion.deny_ids.clone(),
        org_role: expansion.org_role,
        device: None,
        person: person_row,
    }))
}

/// A person principal for unit tests: `person` with `role`, reading `company` and their
/// own `person/<id>`.
#[cfg(test)]
pub(crate) fn test_principal(person: &str, role: margin_shared::OrgRole) -> Principal {
    let mut paths = vec![margin_shared::ROOT_GROUP.to_string(), person_id(person)];
    paths.sort();
    Principal {
        kind: PrincipalKind::Person,
        person_id: person.to_string(),
        id: person_id(person),
        deny_ids: paths.clone(),
        paths,
        org_role: role,
        device: None,
        person: margin_shared::Person {
            id: person.into(),
            handle: person.into(),
            name: person.into(),
            github_login: person.into(),
            github_id: None,
            avatar_url: None,
            org_role: role,
            state: margin_shared::PersonState::Active,
            groups: Vec::new(),
            settings: margin_shared::PersonSettings::default(),
            created_at: Utc::now(),
        },
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

#[cfg(test)]
mod access_tests {
    use super::*;
    use margin_shared::OrgRole;

    fn topic(owner: Option<&str>, access: &[&str]) -> Topic {
        Topic {
            id: "t".into(),
            parent_id: None,
            title: "T".into(),
            position: 0,
            owner: owner.map(Into::into),
            access: access.iter().map(|a| a.to_string()).collect(),
            deny: Vec::new(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn rows_are_private_to_their_owner_unless_shared() {
        let alice = test_principal("alice", OrgRole::Owner);
        let bob = test_principal("bob", OrgRole::Member);
        let mine = topic(Some("alice"), &["person/alice"]);
        assert!(can_read(&alice, &mine) && !can_read(&bob, &mine));
        assert!(check_owner(&alice, &mine).is_ok());
        assert_eq!(check_owner(&bob, &mine).unwrap_err().code, "not_owner");
        // Read empty: private to the owner (defence in depth).
        let empty = topic(Some("alice"), &[]);
        assert_eq!(empty.audience(), ["person/alice"]);
        assert!(!can_read(&bob, &empty));
        // From before `people-v1`: the bootstrap owner's.
        let legacy = topic(None, &[]);
        let boot = test_principal(BOOTSTRAP_PERSON_ID, OrgRole::Owner);
        assert!(can_read(&boot, &legacy) && !can_read(&alice, &legacy));
        // Shared with the company: everyone reads it, only its owner changes it.
        let shared = topic(Some("alice"), &["company"]);
        assert!(can_read(&bob, &shared) && check_owner(&bob, &shared).is_err());
        // A row filed under it copies its owner and access.
        let own = Ownership::of(&shared);
        assert_eq!(own.owner, "alice");
        assert_eq!(own.access, ["company"]);
        assert_eq!(Ownership::private(&bob).access, ["person/bob"]);
    }

    #[test]
    fn only_directory_events_go_to_everyone() {
        assert!(is_directory_event(&Event::GroupDeleted {
            path: "company/x".into()
        }));
        let note = Event::NoteDeleted { id: "n".into() };
        assert!(!is_directory_event(&note));
        // Content without an audience reaches nobody; device events the admins.
        assert_eq!(fallback_audience(&note), Audience::Paths(Vec::new()));
        assert_eq!(
            fallback_audience(&Event::DeviceDeleted { id: "d".into() }),
            Audience::Admins
        );
        assert!(!Audience::Paths(Vec::new()).allows(&test_principal("a", OrgRole::Owner)));
    }
}
