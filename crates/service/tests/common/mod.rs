//! Test harness: the real router on an ephemeral port, in-memory SurrealDB and
//! secrets, a fake GitHub, a fake Hugging Face, and fake Generator/Renderer/SourceTools.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Form, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use margin_agent::scripted::{text_reply, tool_calls_reply};
use margin_agent::{ModelClient, Scripted, ScriptedModel};
use margin_ai::{
    AiError, CallOptions, CoursePlan, Generated, LessonContext, PlanRequest, SceneRequest,
};
use margin_embed::fake::FakeEmbedder;
use margin_embed::{
    Chunk, EmbedError, Embedder, Extracted, ImageTokens, ModelFiles, Page, Picture, Transcript,
    Word,
};
use margin_service::config::Config;
use margin_service::db::Store;
use margin_service::engines::{AiConfig, BoxFuture, Generator, Renderer, SourceTools, SpeechModel};
use margin_service::pipeline::{Chunker, ChunkerCfg};
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore, OPENAI_API_KEY, SecretStore};
use margin_service::server::{Options, Service};
use margin_service::state::AppState;
use margin_shared::{
    AskRequest, Card, ChatDetail, ChatItemKind, ChatState, DeviceFrame, DirectedFrame, Event,
    Level, LoginState, Outline, OutlineLesson, Proposal, ServerFrame, SourceKind, StartLogin,
    ToolStatus,
};
use margin_video::{RenderConfig, RenderError, RenderJob, RenderOutput, RenderProgress};
use serde_json::json;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

pub const CLIENT_ID: &str = "test-client-id";
pub const CLIENT_SECRET: &str = "test-client-secret";
pub const OWNER_CODE: &str = "code-owner";
pub const OTHER_CODE: &str = "code-other";
pub const API_KEY: &str = "sk-test-very-secret-key";

// ---------------------------------------------------------------------------
// Fake GitHub
// ---------------------------------------------------------------------------

pub async fn start_fake_github() -> String {
    async fn token(
        Form(form): Form<std::collections::HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        if form.get("client_id").map(String::as_str) != Some(CLIENT_ID)
            || form.get("client_secret").map(String::as_str) != Some(CLIENT_SECRET)
        {
            return Json(
                json!({"error": "incorrect_client_credentials", "error_description": "bad client"}),
            );
        }
        match form.get("code").map(String::as_str) {
            Some(OWNER_CODE) => Json(
                json!({"access_token": "gho_owner", "token_type": "bearer", "scope": "read:user"}),
            ),
            Some(OTHER_CODE) => Json(
                json!({"access_token": "gho_other", "token_type": "bearer", "scope": "read:user"}),
            ),
            _ => Json(
                json!({"error": "bad_verification_code", "error_description": "The code passed is incorrect or expired."}),
            ),
        }
    }
    async fn user(headers: HeaderMap) -> (axum::http::StatusCode, Json<serde_json::Value>) {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        match auth {
            // Different case from the allowlist on purpose.
            "Bearer gho_owner" => (
                axum::http::StatusCode::OK,
                Json(json!({"login": "nkr1SHNA", "id": 1})),
            ),
            "Bearer gho_other" => (
                axum::http::StatusCode::OK,
                Json(json!({"login": "someone-else", "id": 2})),
            ),
            _ => (
                axum::http::StatusCode::UNAUTHORIZED,
                Json(json!({"message": "Bad credentials"})),
            ),
        }
    }
    let app = Router::new()
        .route("/login/oauth/access_token", post(token))
        .route("/user", get(user));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}")
}

// ---------------------------------------------------------------------------
// Fake generator
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct FakeGenerator {
    pub active: AtomicUsize,
    pub max_active: AtomicUsize,
    pub fail_lessons: Mutex<HashSet<String>>,
    pub scene_requests: Mutex<Vec<SceneRequest>>,
    /// Every lesson request.
    pub lesson_requests: Mutex<Vec<LessonContext>>,
    /// Delay between lesson deltas.
    pub delta_delay_ms: AtomicU32,
    /// When set, every lesson is this body (streamed in two halves) instead of the
    /// default text.
    pub lesson_body: Mutex<Option<String>>,
    /// The model chat turns call: queue replies with `agent.push(..)`.
    pub agent: Arc<ScriptedModel>,
    /// The model chat titles are written with (`Turns::title_model`), apart from
    /// `agent` so a title call never takes a turn's reply. Empty, a title call fails and
    /// the first-line title stays; queue a title with `title_reply`.
    pub titles: Arc<ScriptedModel>,
    /// When set, chat turns call the real `ResponsesClient` at Settings' base URL
    /// instead of `agent` (point it at a fake Responses API), and so does the outline
    /// planner.
    pub agent_over_http: AtomicBool,
    /// Every outline planner request, with its call options.
    pub plan_requests: Mutex<Vec<(PlanRequest, CallOptions)>>,
    /// The planner's answer (checked against the request like the real one); `None`
    /// fails the call, which leaves the agent's outline as it is.
    pub plan_reply: Mutex<Option<CoursePlan>>,
    /// The call options of every lesson and scene request.
    pub call_options: Mutex<Vec<CallOptions>>,
}

/// The usage every fake generation call reports.
pub fn fake_usage() -> margin_shared::Usage {
    margin_shared::Usage {
        input_tokens: 1_000,
        cached_input_tokens: 400,
        output_tokens: 200,
        reasoning_tokens: 0,
        model_calls: 1,
    }
}

pub fn outline_for(req: &AskRequest) -> Outline {
    Outline {
        topic_title: format!("Topic: {}", req.prompt),
        subject_title: "Signals & Systems".into(),
        lessons: (0..req.lesson_count)
            .map(|i| OutlineLesson {
                title: format!("Lesson {}", i + 1),
                summary: format!("Summary {}", i + 1),
                minutes: 5,
            })
            .collect(),
    }
}

impl Generator for FakeGenerator {
    fn write_lesson<'a>(
        &'a self,
        _ai: &'a AiConfig,
        ctx: &'a LessonContext,
        opts: &'a CallOptions,
        mut on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>> {
        Box::pin(async move {
            self.lesson_requests.lock().unwrap().push(ctx.clone());
            self.call_options.lock().unwrap().push(opts.clone());
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            let title = &ctx.outline[ctx.index].title;
            let custom = self.lesson_body.lock().unwrap().clone();
            let parts = match custom {
                Some(body) => {
                    let half = body
                        .char_indices()
                        .nth(body.chars().count() / 2)
                        .map_or(0, |(i, _)| i);
                    vec![body[..half].to_string(), body[half..].to_string()]
                }
                None => vec![
                    format!("# {title}\n\n"),
                    format!(
                        "This lesson of {} / {} ",
                        ctx.subject_title, ctx.topic_title
                    ),
                    "explains the idea step by step.".to_string(),
                ],
            };
            let mut out = String::new();
            let delay = self.delta_delay_ms.load(Ordering::SeqCst).max(20);
            for p in &parts {
                tokio::time::sleep(Duration::from_millis(delay as u64)).await;
                on_delta(p);
                out.push_str(p);
            }
            self.active.fetch_sub(1, Ordering::SeqCst);
            if self.fail_lessons.lock().unwrap().contains(title) {
                return Err(AiError::Incomplete("the model stopped".into()));
            }
            Ok(Generated {
                value: out,
                usage: fake_usage(),
            })
        })
    }

    fn write_scene<'a>(
        &'a self,
        _ai: &'a AiConfig,
        req: &'a SceneRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<String>, AiError>> {
        Box::pin(async move {
            self.scene_requests.lock().unwrap().push(req.clone());
            self.call_options.lock().unwrap().push(opts.clone());
            let n = self.scene_requests.lock().unwrap().len();
            Ok(Generated {
                value: format!(
                    "import manimgx as m\n\nclass LessonScene(m.Scene):\n    # attempt {n}\n    pass\n"
                ),
                usage: fake_usage(),
            })
        })
    }

    fn plan_course<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a PlanRequest,
        opts: &'a CallOptions,
    ) -> BoxFuture<'a, Result<Generated<CoursePlan>, AiError>> {
        Box::pin(async move {
            self.plan_requests
                .lock()
                .unwrap()
                .push((req.clone(), opts.clone()));
            if self.agent_over_http.load(Ordering::SeqCst) {
                let client = margin_ai::ResponsesClient::new(
                    ai.base_url.clone(),
                    ai.api_key.clone(),
                    ai.model.clone(),
                );
                return margin_ai::plan_course(&client, req, opts).await;
            }
            let reply = self.plan_reply.lock().unwrap().clone();
            match reply {
                Some(plan) => Ok(Generated {
                    value: plan.checked(req)?,
                    usage: fake_usage(),
                }),
                None => Err(AiError::Incomplete("no plan scripted".into())),
            }
        })
    }

    fn agent_model(&self, ai: &AiConfig) -> Arc<dyn ModelClient> {
        assert_eq!(ai.api_key, API_KEY);
        if self.agent_over_http.load(Ordering::SeqCst) {
            return Arc::new(margin_ai::ResponsesClient::new(
                ai.base_url.clone(),
                ai.api_key.clone(),
                ai.model.clone(),
            ));
        }
        self.agent.clone()
    }
}

// ---------------------------------------------------------------------------
// Fake renderer
// ---------------------------------------------------------------------------

pub const MP4_BYTES: usize = 4096;

pub fn mp4_content() -> Vec<u8> {
    (0..MP4_BYTES).map(|i| (i % 251) as u8).collect()
}

pub const POSTER_BYTES: usize = 1024;

/// The fake poster: a PNG signature followed by a recognisable byte pattern.
pub fn poster_content() -> Vec<u8> {
    let mut bytes = b"\x89PNG\r\n\x1a\n".to_vec();
    bytes.extend((bytes.len()..POSTER_BYTES).map(|i| (i % 241) as u8));
    bytes
}

#[derive(Default)]
pub struct FakeRenderer {
    pub missing: Mutex<HashSet<String>>,
    /// Number of upcoming renders that fail with `RenderError::Scene`.
    pub scene_failures: AtomicU32,
    pub renders: AtomicU32,
    pub configs: Mutex<Vec<RenderConfig>>,
    /// When set, renders return no poster (like a scene without a poster shot).
    pub no_poster: AtomicBool,
}

impl Renderer for FakeRenderer {
    fn detect_tools(&self) -> BoxFuture<'_, Vec<ToolStatus>> {
        Box::pin(async move {
            let missing = self.missing.lock().unwrap().clone();
            ["manimgx", "fluidaudiocli"]
                .into_iter()
                .map(|name| {
                    let found = !missing.contains(name);
                    ToolStatus {
                        name: name.into(),
                        found,
                        path: found.then(|| format!("/opt/fake/bin/{name}")),
                        version: found.then(|| "1.0".into()),
                        install_commands: vec![format!("install {name}")],
                    }
                })
                .collect()
        })
    }

    fn render<'a>(
        &'a self,
        job: &'a RenderJob,
        config: &'a RenderConfig,
        mut on_progress: Box<dyn FnMut(RenderProgress) + Send + 'a>,
    ) -> BoxFuture<'a, Result<RenderOutput, RenderError>> {
        Box::pin(async move {
            self.renders.fetch_add(1, Ordering::SeqCst);
            self.configs.lock().unwrap().push(config.clone());
            assert!(job.scene_code.contains("LessonScene"));
            assert!(job.work_dir.is_dir(), "work dir exists");
            if self.scene_failures.load(Ordering::SeqCst) > 0 {
                self.scene_failures.fetch_sub(1, Ordering::SeqCst);
                on_progress(RenderProgress::Log("Traceback: NameError on line 3".into()));
                return Err(RenderError::Scene {
                    message: "line 3: NameError: name 'Circl' is not defined".into(),
                });
            }
            for done in 0..=2 {
                on_progress(RenderProgress::Voicing { done, total: 2 });
            }
            for i in 0..250 {
                on_progress(RenderProgress::Log(format!("render line {i}")));
            }
            on_progress(RenderProgress::Rendering {
                progress: Some(0.5),
            });
            tokio::time::sleep(Duration::from_millis(30)).await;
            on_progress(RenderProgress::Rendering {
                progress: Some(1.0),
            });
            let out_dir = job.work_dir.join("media");
            tokio::fs::create_dir_all(&out_dir).await.unwrap();
            let mp4 = out_dir.join("LessonScene.mp4");
            tokio::fs::write(&mp4, mp4_content()).await.unwrap();
            let storyboard = out_dir.join("LessonScene_storyboard.png");
            tokio::fs::write(&storyboard, b"\x89PNG\r\n\x1a\nfake")
                .await
                .unwrap();
            let captions = out_dir.join("LessonScene.srt");
            tokio::fs::write(&captions, "1\n00:00:00,000 --> 00:00:01,000\nHello\n")
                .await
                .unwrap();
            let poster = if self.no_poster.load(Ordering::SeqCst) {
                None
            } else {
                let poster = out_dir.join("LessonScene_poster.png");
                tokio::fs::write(&poster, poster_content()).await.unwrap();
                Some(poster)
            };
            Ok(RenderOutput {
                poster,
                mp4,
                storyboard,
                captions: Some(captions),
                duration_seconds: Some(12.5),
            })
        })
    }
}

// ---------------------------------------------------------------------------
// Fake Hugging Face and source tools
// ---------------------------------------------------------------------------

/// The fake model: one file, sent in `MODEL_PARTS` parts.
pub const MODEL_FILE: &str = "model.bin";
pub const MODEL_PARTS: usize = 8;
pub const MODEL_PART_BYTES: usize = 64 * 1024;
pub const MODEL_BYTES: u64 = (MODEL_PARTS * MODEL_PART_BYTES) as u64;

#[derive(Default)]
pub struct FakeHf {
    /// Pause before each part of the download.
    pub part_delay_ms: AtomicU64,
    /// When set, the download answers 500.
    pub fail: AtomicBool,
    pub size_requests: AtomicU32,
    pub downloads: AtomicU32,
}

fn model_path() -> String {
    format!("/{}/resolve/main/{MODEL_FILE}", margin_embed::REPO)
}

pub async fn start_fake_hf() -> (String, Arc<FakeHf>) {
    async fn size(State(hf): State<Arc<FakeHf>>) -> Json<serde_json::Value> {
        hf.size_requests.fetch_add(1, Ordering::SeqCst);
        Json(json!({"size": MODEL_BYTES}))
    }
    async fn file(State(hf): State<Arc<FakeHf>>) -> Response {
        hf.downloads.fetch_add(1, Ordering::SeqCst);
        if hf.fail.load(Ordering::SeqCst) {
            return (StatusCode::INTERNAL_SERVER_ERROR, "boom").into_response();
        }
        let delay = hf.part_delay_ms.load(Ordering::SeqCst);
        let parts = futures_util::stream::iter(0..MODEL_PARTS).then(move |i| async move {
            tokio::time::sleep(Duration::from_millis(delay)).await;
            Ok::<_, std::convert::Infallible>(Bytes::from(vec![i as u8; MODEL_PART_BYTES]))
        });
        Response::builder()
            .header(header::CONTENT_LENGTH, MODEL_BYTES)
            .body(Body::from_stream(parts))
            .unwrap()
    }
    let hf = Arc::new(FakeHf::default());
    let app = Router::new()
        .route(&format!("/api/models/{}", margin_embed::REPO), get(size))
        .route(
            &format!("/api/models/{}", margin_embed::ONNX_REPO),
            get(size),
        )
        .route(&model_path(), get(file))
        .with_state(hf.clone());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (format!("http://{addr}"), hf)
}

/// Text in a source that makes the fake extraction fail.
pub const UNREADABLE: &str = "%UNREADABLE%";

/// Starts every fake rendered page and video frame (the rest is its text, which
/// `FakeEmbedder` embeds).
pub const FAKE_PICTURE: &str = "PICTURE ";

/// Model files from the fake Hugging Face, `FakeEmbedder` as the model, and simple
/// extraction and chunking, all on UTF-8 test files:
/// - a "PDF" has pages separated by form feeds; each page is also "rendered" to
///   `page-{n}.png` holding `FAKE_PICTURE` and its text;
/// - an "image" is its own picture;
/// - "audio" is the words it says, one a second;
/// - a "video" has `frame: <what it shows>` lines (frames at 2.5 s, 7.5 s, …, written as
///   `frame-{ms}.jpg`) and `say: <words>` lines (its soundtrack);
/// - chunks are paragraphs, and the transcriber hears the words back (unless
///   `no_transcriber` is set, as if `fluidaudiocli` weren't installed).
#[derive(Default)]
pub struct FakeSourceTools {
    pub loads: AtomicU32,
    pub extracts: AtomicU32,
    pub transcribes: AtomicU32,
    pub no_transcriber: AtomicBool,
    /// The chunker each `chunk` call was asked for, by name.
    pub chunkers: Mutex<Vec<String>>,
    /// Pages asked for through `pdf_page_picture`.
    pub page_renders: AtomicU32,
    /// What each fake sound says, by `sound_key`.
    spoken: Mutex<std::collections::HashMap<u64, String>>,
    /// When set, the service downloads a speech model (as on Linux): `speech`.
    pub has_speech_model: AtomicBool,
    pub speech: FakeSpeechModel,
    /// When set, pictures are read without OCR (as on Linux without tesseract).
    pub no_ocr: AtomicBool,
}

/// A speech model of `FAKE_SPEECH_BYTES` "downloaded" in two progress steps, kept in
/// memory.
#[derive(Default)]
pub struct FakeSpeechModel {
    pub downloaded: AtomicBool,
    pub downloads: AtomicU32,
    pub removes: AtomicU32,
    /// The next download fails.
    pub fail: AtomicBool,
}

pub const FAKE_SPEECH_REPO: &str = "istupakov/parakeet-tdt-0.6b-v3-onnx";
pub const FAKE_SPEECH_BYTES: u64 = 670;

impl SpeechModel for FakeSpeechModel {
    fn repo(&self) -> String {
        FAKE_SPEECH_REPO.into()
    }
    fn is_complete(&self) -> bool {
        self.downloaded.load(Ordering::SeqCst)
    }
    fn size_on_disk(&self) -> u64 {
        if self.is_complete() {
            FAKE_SPEECH_BYTES
        } else {
            0
        }
    }
    fn remote_size<'a>(
        &'a self,
        _: &'a reqwest::Client,
        _: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>> {
        Box::pin(async { Ok(FAKE_SPEECH_BYTES) })
    }
    fn download<'a>(
        &'a self,
        _: &'a reqwest::Client,
        _: &'a str,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), EmbedError>> {
        Box::pin(async move {
            self.downloads.fetch_add(1, Ordering::SeqCst);
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(EmbedError::Download("offline".into()));
            }
            progress(FAKE_SPEECH_BYTES / 2, FAKE_SPEECH_BYTES);
            tokio::time::sleep(Duration::from_millis(600)).await;
            progress(FAKE_SPEECH_BYTES, FAKE_SPEECH_BYTES);
            self.downloaded.store(true, Ordering::SeqCst);
            Ok(())
        })
    }
    fn remove(&self) -> Result<(), EmbedError> {
        self.removes.fetch_add(1, Ordering::SeqCst);
        self.downloaded.store(false, Ordering::SeqCst);
        Ok(())
    }
}

/// Identifies fake sound by its samples.
fn sound_key(samples: &[f32]) -> u64 {
    samples.iter().fold(0xcbf29ce484222325u64, |h, s| {
        (h ^ s.to_bits() as u64).wrapping_mul(0x100000001b3)
    })
}

impl FakeSourceTools {
    /// 16 kHz "sound" of `text`, one second per word, remembered for the transcriber.
    fn speak(&self, text: &str) -> Vec<f32> {
        let mut samples = Vec::new();
        for word in text.split_whitespace() {
            let level = (word.bytes().map(u32::from).sum::<u32>() % 1000) as f32 / 2000.0;
            samples.extend(std::iter::repeat_n(level, 16_000));
        }
        self.spoken
            .lock()
            .unwrap()
            .insert(sound_key(&samples), text.to_string());
        samples
    }
}

impl SourceTools for FakeSourceTools {
    fn is_complete(&self, files: &ModelFiles) -> bool {
        files.dir.join(MODEL_FILE).is_file()
    }

    fn size_on_disk(&self, files: &ModelFiles) -> u64 {
        std::fs::metadata(files.dir.join(MODEL_FILE)).map_or(0, |m| m.len())
    }

    fn remote_size<'a>(
        &'a self,
        files: &'a ModelFiles,
        http: &'a reqwest::Client,
        hf_base: &'a str,
    ) -> BoxFuture<'a, Result<u64, EmbedError>> {
        Box::pin(async move {
            let url = format!("{hf_base}/api/models/{}", files.backend.repo());
            let v: serde_json::Value = http
                .get(url)
                .send()
                .await
                .map_err(|e| EmbedError::Download(e.to_string()))?
                .json()
                .await
                .map_err(|e| EmbedError::Download(e.to_string()))?;
            Ok(v["size"].as_u64().unwrap())
        })
    }

    fn download<'a>(
        &'a self,
        files: &'a ModelFiles,
        http: &'a reqwest::Client,
        hf_base: &'a str,
        progress: &'a (dyn Fn(u64, u64) + Send + Sync),
    ) -> BoxFuture<'a, Result<(), EmbedError>> {
        Box::pin(async move {
            let r = http
                .get(format!("{hf_base}{}", model_path()))
                .send()
                .await
                .map_err(|e| EmbedError::Download(e.to_string()))?;
            if !r.status().is_success() {
                return Err(EmbedError::Download(format!("HTTP {}", r.status())));
            }
            let total = r.content_length().unwrap_or(0);
            tokio::fs::create_dir_all(&files.dir).await?;
            let mut data = Vec::new();
            let mut body = r.bytes_stream();
            while let Some(part) = body.next().await {
                let part = part.map_err(|e| EmbedError::Download(e.to_string()))?;
                data.extend_from_slice(&part);
                progress(data.len() as u64, total);
            }
            let part = files.dir.join(format!("{MODEL_FILE}.part"));
            tokio::fs::write(&part, &data).await?;
            tokio::fs::rename(&part, files.dir.join(MODEL_FILE)).await?;
            Ok(())
        })
    }

    fn remove(&self, files: &ModelFiles) -> Result<(), EmbedError> {
        match std::fs::remove_dir_all(&files.dir) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    fn load(&self, files: &ModelFiles) -> Result<Arc<dyn Embedder>, EmbedError> {
        assert!(
            self.is_complete(files),
            "load is only called once downloaded"
        );
        self.loads.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(FakeEmbedder))
    }

    fn extract(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError> {
        self.extracts.fetch_add(1, Ordering::SeqCst);
        let text = String::from_utf8_lossy(&std::fs::read(path)?).into_owned();
        if text.contains(UNREADABLE) {
            return Err(EmbedError::Extract("the file is damaged".into()));
        }
        let picture = |path: PathBuf, page, at_ms, tokens| Picture {
            path,
            page,
            at_ms,
            tokens,
        };
        let mut doc = Extracted::default();
        match kind {
            SourceKind::Pdf => {
                std::fs::create_dir_all(media_dir)?;
                for (i, t) in text.split('\x0c').enumerate() {
                    let n = i as u32 + 1;
                    let file = media_dir.join(margin_embed::page_file(n));
                    std::fs::write(&file, format!("{FAKE_PICTURE}{t}"))?;
                    doc.pictures
                        .push(picture(file, Some(n), None, ImageTokens::High));
                    doc.pages.push(Page {
                        number: Some(n),
                        text: t.to_string(),
                    });
                }
            }
            SourceKind::Image => {
                doc.pictures
                    .push(picture(path.to_path_buf(), None, None, ImageTokens::High));
            }
            SourceKind::Audio => {
                doc.sound = Some(self.speak(&text));
                doc.duration_ms = Some(text.split_whitespace().count() as u64 * 1000);
            }
            SourceKind::Video => {
                std::fs::create_dir_all(media_dir)?;
                let mut said = String::new();
                let mut frames = 0u64;
                for line in text.lines() {
                    if let Some(f) = line.strip_prefix("frame: ") {
                        let ms = 2_500 + 5_000 * frames;
                        let file = media_dir.join(margin_embed::frame_file(ms));
                        std::fs::write(&file, format!("{FAKE_PICTURE}{f}"))?;
                        doc.pictures
                            .push(picture(file, None, Some(ms), ImageTokens::Medium));
                        frames += 1;
                    } else if let Some(s) = line.strip_prefix("say: ") {
                        said.push_str(s);
                        said.push(' ');
                    }
                }
                if !said.trim().is_empty() {
                    doc.sound = Some(self.speak(&said));
                }
                doc.duration_ms = Some(frames * 5_000);
            }
            SourceKind::Text | SourceKind::Code => doc.pages.push(Page { number: None, text }),
        }
        Ok(doc)
    }

    /// As `extract`, with no page pictures for a PDF (ingest asks `pdf_page_picture`).
    fn extract_lazy(
        &self,
        path: &Path,
        kind: SourceKind,
        media_dir: &Path,
    ) -> Result<Extracted, EmbedError> {
        let mut doc = self.extract(path, kind, media_dir)?;
        if kind == SourceKind::Pdf {
            for p in doc.pictures.drain(..) {
                std::fs::remove_file(&p.path)?;
            }
        }
        Ok(doc)
    }

    fn transcribe(
        &self,
        samples: &[f32],
        _work_dir: &Path,
    ) -> Result<Option<Transcript>, EmbedError> {
        self.transcribes.fetch_add(1, Ordering::SeqCst);
        if self.no_transcriber.load(Ordering::SeqCst)
            || (self.has_speech_model.load(Ordering::SeqCst) && !self.speech.is_complete())
        {
            return Ok(None);
        }
        let said = self
            .spoken
            .lock()
            .unwrap()
            .get(&sound_key(samples))
            .cloned();
        let Some(said) = said else {
            return Err(EmbedError::Transcribe("the speech is unclear".into()));
        };
        Ok(Some(Transcript {
            words: said
                .split_whitespace()
                .enumerate()
                .map(|(i, w)| Word {
                    text: w.to_string(),
                    start_ms: i as u64 * 1000,
                    end_ms: i as u64 * 1000 + 800,
                })
                .collect(),
        }))
    }

    fn no_transcriber(&self) -> String {
        if self.has_speech_model.load(Ordering::SeqCst) {
            "the speech model isn't downloaded (Settings → Sources downloads it)".into()
        } else {
            "fluidaudiocli isn't installed on the service (Settings → Tools shows how to install it)".into()
        }
    }

    fn speech_model(&self) -> Option<&dyn SpeechModel> {
        self.has_speech_model
            .load(Ordering::SeqCst)
            .then_some(&self.speech as &dyn SpeechModel)
    }

    fn ocr_missing(&self) -> Option<String> {
        self.no_ocr.load(Ordering::SeqCst).then(|| {
            "tesseract isn't installed on the service (Settings → Tools shows how to install it)"
                .to_string()
        })
    }

    fn image_for_model(&self, path: &Path) -> Result<(String, Vec<u8>), EmbedError> {
        Ok(("image/jpeg".into(), std::fs::read(path)?))
    }

    fn chunk(
        &self,
        chunker: &dyn Chunker,
        doc: &Extracted,
        _kind: SourceKind,
        count_tokens: &dyn Fn(&str) -> usize,
        cfg: &ChunkerCfg,
    ) -> Vec<Chunk> {
        self.chunkers
            .lock()
            .unwrap()
            .push(chunker.name().to_string());
        let mut chunks = Vec::new();
        for page in &doc.pages {
            // Lines are counted from the page's first line, paragraphs being separated by
            // one blank line; a "# " line names the section of what follows.
            let mut line = 1u32;
            let mut section = String::new();
            for raw in page.text.split("\n\n") {
                let lines = raw.lines().count().max(1) as u32;
                let (start, end) = (line, line + lines - 1);
                line += lines + 1;
                let para = raw.trim();
                if para.is_empty() {
                    continue;
                }
                if let Some(heading) = para.strip_prefix("# ") {
                    section = heading.trim().to_string();
                    continue;
                }
                assert!(
                    count_tokens(para) <= cfg.max_tokens,
                    "test paragraphs are short"
                );
                chunks.push(Chunk {
                    text: para.to_string(),
                    page: page.number,
                    section_path: section.clone(),
                    start_line: page.number.is_none().then_some(start),
                    end_line: page.number.is_none().then_some(end),
                    ..Default::default()
                });
            }
        }
        chunks
    }

    fn pdf_page_picture(
        &self,
        pdf: &Path,
        page: u32,
        media_dir: &Path,
    ) -> Result<PathBuf, EmbedError> {
        self.page_renders.fetch_add(1, Ordering::SeqCst);
        let target = media_dir.join(margin_embed::page_file(page));
        if target.is_file() {
            return Ok(target);
        }
        let text = String::from_utf8_lossy(&std::fs::read(pdf)?).into_owned();
        let t = text
            .split('\x0c')
            .nth(page as usize - 1)
            .ok_or_else(|| EmbedError::Extract(format!("the PDF has no page {page}")))?;
        std::fs::create_dir_all(media_dir)?;
        std::fs::write(&target, format!("{FAKE_PICTURE}{t}"))?;
        Ok(target)
    }

    fn score(
        &self,
        query_vec: &[f32],
        _query_text: &str,
        chunk_vec: &[f32],
        _chunk_text: &str,
    ) -> f32 {
        margin_embed::cosine(query_vec, chunk_vec)
    }
}

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

pub struct Harness {
    pub url: String,
    pub http: reqwest::Client,
    pub state: AppState,
    pub secrets: Arc<MemoryStore>,
    pub generator: Arc<FakeGenerator>,
    pub renderer: Arc<FakeRenderer>,
    pub source_tools: Arc<FakeSourceTools>,
    pub hf: Arc<FakeHf>,
    pub data_dir: PathBuf,
    pub stop: Option<tokio::sync::oneshot::Sender<()>>,
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.data_dir);
    }
}

pub struct Setup {
    pub store: Option<Store>,
    pub secrets: MemoryStore,
    /// Whether the embedding model is already downloaded.
    pub model_downloaded: bool,
    /// Files to put in the data folder before starting, by path inside it.
    pub files: Vec<(String, Vec<u8>)>,
    /// A downloaded real model: when set, the service reads sources with the real
    /// `EmbedTools` (Core ML, PDFKit, ImageIO, AVFoundation, fluidaudiocli) and this
    /// model, linked into its data folder, instead of the fakes.
    pub real_model: Option<PathBuf>,
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            store: None,
            secrets: MemoryStore::new().with(GITHUB_CLIENT_SECRET, CLIENT_SECRET),
            model_downloaded: true,
            files: Vec::new(),
            real_model: None,
        }
    }
}

pub async fn start() -> Harness {
    start_with(Setup::default()).await
}

pub async fn start_with(setup: Setup) -> Harness {
    let github = start_fake_github().await;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let url = format!("http://{addr}");
    let data_dir =
        std::env::temp_dir().join(format!("margin-service-test-{}", uuid::Uuid::new_v4()));
    let config = Config {
        public_url: url.clone(),
        bind: addr.to_string(),
        data_dir: data_dir.clone(),
        github_client_id: CLIENT_ID.into(),
        allowed_github_login: "Nkr1shna".into(),
        github_api_url: github.clone(),
        github_oauth_url: github,
        environment: "dev".into(),
        ..Config::default()
    };
    let store = match setup.store {
        Some(s) => s,
        None => Store::memory().await.unwrap(),
    };
    if let Some(real) = &setup.real_model {
        let model = config.embedding_model_dir();
        std::fs::create_dir_all(model.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(real, &model).unwrap();
    } else if setup.model_downloaded {
        let model = config.embedding_model_dir();
        std::fs::create_dir_all(&model).unwrap();
        std::fs::write(model.join(MODEL_FILE), vec![0u8; MODEL_BYTES as usize]).unwrap();
    }
    for (path, bytes) in &setup.files {
        let path = data_dir.join(path);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
    }
    let (hf_base, hf) = start_fake_hf().await;
    let secrets = Arc::new(setup.secrets);
    let generator = Arc::new(FakeGenerator::default());
    let renderer = Arc::new(FakeRenderer::default());
    let source_tools = Arc::new(FakeSourceTools::default());
    let models_dir = config.models_dir();
    let service = Service::start(Options {
        config,
        store,
        secrets: secrets.clone(),
        generator: generator.clone(),
        renderer: renderer.clone(),
        source_tools: match setup.real_model {
            Some(_) => Arc::new(margin_service::engines::EmbedTools::new(&models_dir)),
            None => source_tools.clone(),
        },
        hf_base,
        allow_unsandboxed_render: true,
    })
    .await
    .unwrap();
    let state = service.state.clone();
    *state.turns.title_model.lock().unwrap() = Some(generator.titles.clone());
    let (tx, rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        service
            .serve(listener, async move {
                let _ = rx.await;
            })
            .await
            .unwrap()
    });
    let http = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap();
    Harness {
        url,
        http,
        state,
        secrets,
        generator,
        renderer,
        source_tools,
        hf,
        data_dir,
        stop: Some(tx),
    }
}

impl Harness {
    pub fn u(&self, path: &str) -> String {
        format!("{}{path}", self.url)
    }

    /// `POST /api/sources` with the file's bytes.
    pub async fn upload(
        &self,
        token: &str,
        name: &str,
        topic_id: Option<&str>,
        bytes: impl Into<reqwest::Body>,
    ) -> reqwest::Response {
        let mut query = vec![("name", name)];
        if let Some(t) = topic_id {
            query.push(("topic_id", t));
        }
        self.post(token, "/api/sources")
            .query(&query)
            .header("content-type", "application/octet-stream")
            .body(bytes)
            .send()
            .await
            .unwrap()
    }

    pub async fn start_login(&self, device: &str) -> StartLogin {
        let r = self
            .http
            .post(self.u("/auth/start"))
            .json(&json!({"device_name": device}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json().await.unwrap()
    }

    pub fn state_param(start: &StartLogin) -> String {
        let url = reqwest::Url::parse(&start.authorize_url).unwrap();
        url.query_pairs()
            .find(|(k, _)| k == "state")
            .unwrap()
            .1
            .to_string()
    }

    pub async fn callback(&self, code: &str, state: &str) -> reqwest::Response {
        self.http
            .get(self.u("/auth/github/callback"))
            .query(&[("code", code), ("state", state)])
            .send()
            .await
            .unwrap()
    }

    pub async fn poll(&self, start: &StartLogin) -> LoginState {
        let r = self
            .http
            .post(self.u("/auth/poll"))
            .json(&json!({"login_id": start.login_id, "poll_secret": start.poll_secret}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
        r.json().await.unwrap()
    }

    /// Full sign-in; returns a device token.
    pub async fn sign_in(&self, device: &str) -> String {
        let s = self.start_login(device).await;
        let r = self.callback(OWNER_CODE, &Self::state_param(&s)).await;
        assert_eq!(r.status(), 200);
        match self.poll(&s).await {
            LoginState::Approved { token, .. } => token,
            other => panic!("expected approval, got {other:?}"),
        }
    }

    pub fn get(&self, token: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.get(self.u(path)).bearer_auth(token)
    }
    pub fn post(&self, token: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.post(self.u(path)).bearer_auth(token)
    }
    pub fn patch(&self, token: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.patch(self.u(path)).bearer_auth(token)
    }
    pub fn delete(&self, token: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.delete(self.u(path)).bearer_auth(token)
    }
    pub fn put(&self, token: &str, path: &str) -> reqwest::RequestBuilder {
        self.http.put(self.u(path)).bearer_auth(token)
    }

    /// Stores an API key and a model.
    pub async fn configure_ai(&self, token: &str) {
        let r = self
            .patch(token, "/api/settings")
            .json(&json!({"openai_api_key": API_KEY, "openai_model": "test-model"}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    }

    /// Queues the reply of the next title call (see `FakeGenerator::titles`).
    pub fn title_reply(&self, reply: &str) {
        self.generator
            .titles
            .push(Scripted::Events(text_reply(&[reply])));
    }

    pub async fn socket(&self, token: &str) -> Socket {
        let mut req = format!("{}/api/events", self.url.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let mut s = Socket {
            ws,
            events: Default::default(),
            directed: Default::default(),
        };
        match s.next_event().await {
            Some(Event::Hello { server_version }) => assert!(!server_version.is_empty()),
            other => panic!("expected Hello, got {other:?}"),
        }
        s
    }
}

/// The arguments of the `propose_course` call that proposes `req` with
/// `outline_for(req)`.
pub fn propose_args(req: &AskRequest) -> serde_json::Value {
    let outline = outline_for(req);
    json!({
        "prompt": req.prompt,
        "topic_title": outline.topic_title,
        "subject_title": outline.subject_title,
        "lessons": outline.lessons.iter().map(|l| json!({
            "title": l.title,
            "summary": l.summary,
            "minutes": l.minutes,
        })).collect::<Vec<_>>(),
        "level": req.level,
        "with_videos": req.with_videos,
        "with_quiz": req.with_quiz,
        "parent_topic_id": req.parent_topic_id,
        "source_ids": req.source_ids,
    })
}

impl Harness {
    /// Proposes a course the way the agent does: a new chat whose model calls
    /// `propose_course` with `req` (outline `outline_for(req)`) and then answers.
    /// Returns the proposal and the chat, or the tool's error (the result row's
    /// summary).
    pub async fn propose_in_chat(
        &self,
        token: &str,
        req: &AskRequest,
    ) -> Result<(Proposal, ChatDetail), String> {
        let args = propose_args(req).to_string();
        self.generator
            .agent
            .push(Scripted::Events(tool_calls_reply(&[(
                "call_propose",
                "propose_course",
                &args,
            )])));
        self.generator
            .agent
            .push(Scripted::Events(text_reply(&["Here is an outline."])));
        let r = self
            .post(token, "/api/chats")
            .json(&json!({"message": {"text": format!("A course, please: {}", req.prompt)}}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        let chat: ChatDetail = r.json().await.unwrap();
        let detail = eventually(|| async {
            let d: ChatDetail = self
                .get(token, &format!("/api/chats/{}", chat.chat.id))
                .send()
                .await
                .unwrap()
                .json()
                .await
                .unwrap();
            (d.chat.state == ChatState::Idle).then_some(d)
        })
        .await;
        let result = detail.items.iter().find_map(|i| match &i.kind {
            ChatItemKind::ToolResult {
                call_id,
                ok,
                summary,
                ..
            } if call_id == "call_propose" => Some((*ok, summary.clone())),
            _ => None,
        });
        match result {
            Some((true, _)) => {}
            Some((false, summary)) => return Err(summary),
            None => panic!("propose_course didn't run: {:?}", detail.items),
        }
        let proposal_id = detail
            .items
            .iter()
            .find_map(|i| match &i.kind {
                ChatItemKind::Card {
                    card: Card::Outline(c),
                    ..
                } => Some(c.proposal_id.clone()),
                _ => None,
            })
            .expect("an outline card");
        let proposal: Proposal = self
            .get(token, &format!("/api/proposals/{proposal_id}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        Ok((proposal, detail))
    }

    /// `propose_in_chat`'s proposal; panics if the tool failed.
    pub async fn propose(&self, token: &str, req: &AskRequest) -> Proposal {
        match self.propose_in_chat(token, req).await {
            Ok((p, _)) => p,
            Err(e) => panic!("propose_course failed: {e}"),
        }
    }
}

pub fn ask_request(prompt: &str, n: u32, with_videos: bool) -> AskRequest {
    AskRequest {
        prompt: prompt.into(),
        lesson_count: n,
        level: Level::Intermediate,
        with_videos,
        with_quiz: true,
        parent_topic_id: None,
        source_ids: vec![],
    }
}

pub struct Socket {
    pub ws: tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    /// Events read while looking for a directed frame, returned first by `next_frame`.
    events: std::collections::VecDeque<Event>,
    /// Directed frames read while looking for an event, returned first by
    /// `next_directed`.
    directed: std::collections::VecDeque<DirectedFrame>,
}

#[allow(clippy::large_enum_variant)] // test helper; size is irrelevant
pub enum Frame {
    Event(Event),
    Closed(Option<u16>),
}

#[allow(clippy::large_enum_variant)] // test helper; size is irrelevant
enum Raw {
    Server(ServerFrame),
    Closed(Option<u16>),
}

impl Socket {
    /// Next text frame (or close), skipping pings; panics after 10 s.
    async fn next_raw(&mut self) -> Raw {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("socket timed out");
            match msg {
                Some(Ok(WsMessage::Text(t))) => {
                    return Raw::Server(serde_json::from_str(t.as_str()).unwrap());
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    return Raw::Closed(frame.map(|f| u16::from(f.code)));
                }
                None | Some(Err(_)) => return Raw::Closed(None),
                Some(Ok(_)) => {}
            }
        }
    }

    /// Next frame (event or close), skipping pings and keeping directed frames for
    /// `next_directed`; panics after 10 s.
    pub async fn next_frame(&mut self) -> Frame {
        if let Some(e) = self.events.pop_front() {
            return Frame::Event(e);
        }
        loop {
            match self.next_raw().await {
                Raw::Server(ServerFrame::Event(e)) => return Frame::Event(e),
                Raw::Server(ServerFrame::Directed(d)) => self.directed.push_back(d),
                Raw::Closed(code) => return Frame::Closed(code),
            }
        }
    }

    /// Next frame addressed to this device, keeping events for `next_frame`; panics
    /// after 10 s or when the socket closes.
    pub async fn next_directed(&mut self) -> DirectedFrame {
        if let Some(d) = self.directed.pop_front() {
            return d;
        }
        loop {
            match self.next_raw().await {
                Raw::Server(ServerFrame::Directed(d)) => return d,
                Raw::Server(ServerFrame::Event(e)) => self.events.push_back(e),
                Raw::Closed(code) => panic!("socket closed ({code:?})"),
            }
        }
    }

    /// Skips events until the socket closes; returns the close code.
    pub async fn closed(&mut self) -> Option<u16> {
        loop {
            if let Frame::Closed(code) = self.next_frame().await {
                return code;
            }
        }
    }

    /// Sends a device frame (a tool result).
    pub async fn send_frame(&mut self, frame: &DeviceFrame) {
        let text = serde_json::to_string(frame).unwrap();
        self.ws.send(WsMessage::Text(text.into())).await.unwrap();
    }

    pub async fn next_event(&mut self) -> Option<Event> {
        match self.next_frame().await {
            Frame::Event(e) => Some(e),
            Frame::Closed(_) => None,
        }
    }

    /// Collects events until `done` returns true for one (included).
    pub async fn until(&mut self, mut done: impl FnMut(&Event) -> bool) -> Vec<Event> {
        let mut out = Vec::new();
        loop {
            let e = self.next_event().await.expect("socket closed early");
            let stop = done(&e);
            out.push(e);
            if stop {
                return out;
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self.ws.close(None).await;
    }

    pub async fn send_ping(&mut self) {
        let _ = self.ws.send(WsMessage::Ping(Default::default())).await;
    }
}

pub fn has_key(secrets: &MemoryStore) -> bool {
    secrets.get(OPENAI_API_KEY).unwrap().is_some()
}

/// Polls `f` every 20 ms until it returns Some, for up to 10 s.
pub async fn eventually<T, F, Fut>(mut f: F) -> T
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Option<T>>,
{
    for _ in 0..500 {
        if let Some(v) = f().await {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("condition not met in time");
}
