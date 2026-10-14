//! Test harness: the real router on an ephemeral port, in-memory SurrealDB and
//! secrets, a fake GitHub, and fake Generator/Renderer.
#![allow(dead_code)]

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::Form;
use axum::http::HeaderMap;
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use margin_ai::{AiError, LessonContext, SceneRequest};
use margin_service::config::Config;
use margin_service::db::Store;
use margin_service::engines::{AiConfig, BoxFuture, Generator, Renderer};
use margin_service::secrets::{GITHUB_CLIENT_SECRET, MemoryStore, OPENAI_API_KEY, SecretStore};
use margin_service::server::{Options, Service};
use margin_service::state::AppState;
use margin_shared::{
    AskRequest, Event, Level, LoginState, Outline, OutlineLesson, StartLogin, ToolStatus,
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
    pub subjects_seen: Mutex<Vec<Vec<String>>>,
    /// Delay between lesson deltas.
    pub delta_delay_ms: AtomicU32,
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
    fn propose_outline<'a>(
        &'a self,
        ai: &'a AiConfig,
        req: &'a AskRequest,
        existing_subjects: &'a [String],
    ) -> BoxFuture<'a, Result<Outline, AiError>> {
        Box::pin(async move {
            assert_eq!(ai.api_key, API_KEY);
            self.subjects_seen
                .lock()
                .unwrap()
                .push(existing_subjects.to_vec());
            tokio::time::sleep(Duration::from_millis(30)).await;
            if req.prompt.contains("FAIL") {
                return Err(AiError::Http {
                    status: 500,
                    message: "boom".into(),
                });
            }
            Ok(outline_for(req))
        })
    }

    fn write_lesson<'a>(
        &'a self,
        _ai: &'a AiConfig,
        ctx: &'a LessonContext,
        mut on_delta: Box<dyn FnMut(&str) + Send + 'a>,
    ) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move {
            let now = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(now, Ordering::SeqCst);
            let title = &ctx.outline[ctx.index].title;
            let parts = [
                format!("# {title}\n\n"),
                format!(
                    "This lesson of {} / {} ",
                    ctx.subject_title, ctx.topic_title
                ),
                "explains the idea step by step.".to_string(),
            ];
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
            Ok(out)
        })
    }

    fn write_scene<'a>(
        &'a self,
        _ai: &'a AiConfig,
        req: &'a SceneRequest,
    ) -> BoxFuture<'a, Result<String, AiError>> {
        Box::pin(async move {
            self.scene_requests.lock().unwrap().push(req.clone());
            let n = self.scene_requests.lock().unwrap().len();
            Ok(format!(
                "import manimgx as m\n\nclass LessonScene(m.Scene):\n    # attempt {n}\n    pass\n"
            ))
        })
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
// Harness
// ---------------------------------------------------------------------------

pub struct Harness {
    pub url: String,
    pub http: reqwest::Client,
    pub state: AppState,
    pub secrets: Arc<MemoryStore>,
    pub generator: Arc<FakeGenerator>,
    pub renderer: Arc<FakeRenderer>,
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
}

impl Default for Setup {
    fn default() -> Self {
        Self {
            store: None,
            secrets: MemoryStore::new().with(GITHUB_CLIENT_SECRET, CLIENT_SECRET),
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
    };
    let store = match setup.store {
        Some(s) => s,
        None => Store::memory().await.unwrap(),
    };
    let secrets = Arc::new(setup.secrets);
    let generator = Arc::new(FakeGenerator::default());
    let renderer = Arc::new(FakeRenderer::default());
    let service = Service::start(Options {
        config,
        store,
        secrets: secrets.clone(),
        generator: generator.clone(),
        renderer: renderer.clone(),
        allow_unsandboxed_render: true,
    })
    .await
    .unwrap();
    let state = service.state.clone();
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
        data_dir,
        stop: Some(tx),
    }
}

impl Harness {
    pub fn u(&self, path: &str) -> String {
        format!("{}{path}", self.url)
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

    pub async fn socket(&self, token: &str) -> Socket {
        let mut req = format!("{}/api/events", self.url.replace("http://", "ws://"))
            .into_client_request()
            .unwrap();
        req.headers_mut()
            .insert("authorization", format!("Bearer {token}").parse().unwrap());
        let (ws, _) = tokio_tungstenite::connect_async(req).await.unwrap();
        let mut s = Socket { ws };
        match s.next_event().await {
            Some(Event::Hello { server_version }) => assert!(!server_version.is_empty()),
            other => panic!("expected Hello, got {other:?}"),
        }
        s
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
}

#[allow(clippy::large_enum_variant)] // test helper; size is irrelevant
pub enum Frame {
    Event(Event),
    Closed(Option<u16>),
}

impl Socket {
    /// Next frame (event or close), skipping pings; panics after 10 s.
    pub async fn next_frame(&mut self) -> Frame {
        loop {
            let msg = tokio::time::timeout(Duration::from_secs(10), self.ws.next())
                .await
                .expect("socket timed out");
            match msg {
                Some(Ok(WsMessage::Text(t))) => {
                    return Frame::Event(serde_json::from_str(t.as_str()).unwrap());
                }
                Some(Ok(WsMessage::Close(frame))) => {
                    return Frame::Closed(frame.map(|f| u16::from(f.code)));
                }
                None | Some(Err(_)) => return Frame::Closed(None),
                Some(Ok(_)) => {}
            }
        }
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
