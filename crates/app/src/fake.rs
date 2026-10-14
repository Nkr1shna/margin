//! A small in-memory stand-in for the Margin service, used by the app's tests and by
//! `cargo run --example fake_service` to drive the real app by hand. It implements the
//! routes in `margin_shared::routes` closely enough to exercise every screen; it is not
//! the service.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, TimeZone, Utc};
use margin_shared::*;
use tokio::sync::broadcast;

pub const TOKEN: &str = "test-device-token";

pub struct FakeState {
    pub device_name: String,
    /// Approve the pending login on the next poll.
    pub approve: bool,
    /// Approve automatically after this many polls (manual runs).
    pub auto_approve_after: Option<u32>,
    /// Deny instead of approving.
    pub deny: Option<String>,
    pub polls: u32,
    pub token_collected: bool,
    pub has_api_key: bool,
    pub library: Library,
    pub proposals: HashMap<Id, Proposal>,
    pub settings: Settings,
    pub devices: Vec<Device>,
    pub next_id: u64,
    /// Simulate generation over time and publish events (manual runs).
    pub simulate: bool,
    pub public_url: String,
    pub media_dir: Option<std::path::PathBuf>,
    pub embedding: EmbeddingModelState,
}

/// Hugging Face repo of the embedding model, as the service reports it.
pub const EMBEDDING_REPO: &str = "FluidInference/embeddinggemma-2-coreml";
/// What the fake says the model download weighs.
pub const EMBEDDING_DOWNLOAD_BYTES: u64 = 640_000_000;

#[derive(Clone)]
pub struct Fake {
    pub state: Arc<Mutex<FakeState>>,
    pub events: broadcast::Sender<Event>,
}

pub struct FakeService {
    pub url: String,
    pub state: Arc<Mutex<FakeState>>,
    pub events: broadcast::Sender<Event>,
}

fn ts(n: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_760_000_000 + n, 0).unwrap()
}

pub const LESSON_ONE: &str = r#"Any periodic signal that's reasonably well behaved can be rebuilt from sine and cosine waves. Their frequencies are whole-number multiples of one base frequency.

## The idea

The slowest wave sets the period $T$. The faster ones repeat a whole number of times inside each period, so they add detail without breaking the repetition. That's why the frequencies must be whole-number multiples, and it's what makes the decomposition unique.

$$f(t) = \frac{a_0}{2} + \sum_{n=1}^{\infty} \left( a_n \cos n\omega_0 t + b_n \sin n\omega_0 t \right), \quad \omega_0 = \frac{2\pi}{T}$$

Each pair $a_n, b_n$ says how much of the $n$-th harmonic the signal contains, so sines & cosines together describe the whole signal.

## Building a square wave

A square wave of height 1 uses only odd harmonics, and only sines:

$$f(t) = \frac{4}{\pi}\left[\sin \omega_0 t + \tfrac{1}{3}\sin 3\omega_0 t + \tfrac{1}{5}\sin 5\omega_0 t + \dots\right]$$

With three terms the shape is already recognisable. With fifty, only a small overshoot at each jump remains. That overshoot, about 9% of the jump, never goes away.

> **Key idea.** Every term repeats with period $T$ because its frequency is an integer multiple of $\omega_0$.

## Check yourself

<details><summary>Why does the square wave have no cosine terms?</summary>

Centred at zero, it's an odd function: $f(-t) = -f(t)$. Cosines are even, so their coefficients integrate to zero.

</details>

<details><summary>If you double the period T, what happens to ω₀?</summary>

It halves, since $\omega_0 = 2\pi / T$.

</details>
"#;

pub fn sample_library() -> Library {
    let topic = |id: &str, parent: Option<&str>, title: &str, position| Topic {
        id: id.into(),
        parent_id: parent.map(Into::into),
        title: title.into(),
        position,
        created_at: ts(0),
        updated_at: ts(0),
    };
    let lesson = |id: &str, topic_id: &str, title: &str, position, status, body: &str| Lesson {
        citations: Vec::new(),
        id: id.into(),
        topic_id: topic_id.into(),
        title: title.into(),
        summary: format!("About {title}"),
        position,
        level: Level::Intro,
        minutes: 9,
        body_md: body.into(),
        status,
        created_at: ts(position),
        updated_at: ts(position),
    };
    Library {
        topics: vec![
            topic("signals", None, "Signals & Systems", 0),
            topic("complex", Some("signals"), "Complex numbers", 0),
            topic("fourier", Some("signals"), "Fourier series", 1),
            topic("linalg", None, "Linear Algebra", 1),
        ],
        lessons: vec![
            lesson(
                "l1",
                "fourier",
                "Why sums of sines?",
                0,
                LessonStatus::Ready,
                LESSON_ONE,
            ),
            lesson(
                "l2",
                "fourier",
                "Computing coefficients",
                1,
                LessonStatus::Ready,
                "## Orthogonality\n\nThe integral of $\\sin m t \\sin n t$ over a period is zero unless $m = n$.\n",
            ),
            lesson(
                "l3",
                "fourier",
                "Convergence & Gibbs",
                2,
                LessonStatus::Failed {
                    error: "The model stopped before finishing the lesson.".into(),
                },
                "",
            ),
            lesson(
                "l4",
                "complex",
                "Euler's formula",
                0,
                LessonStatus::Ready,
                "$$e^{i\\theta} = \\cos\\theta + i\\sin\\theta$$\n",
            ),
        ],
        notes: vec![
            Note {
                id: "n1".into(),
                topic_id: "fourier".into(),
                lesson_id: Some("l1".into()),
                title: "Whole-number multiples".into(),
                body_md: "Non-integer ratios wouldn't repeat with period T.".into(),
                quote: Some("whole-number multiples".into()),
                created_at: ts(10),
                updated_at: ts(10),
            },
            Note {
                id: "n2".into(),
                topic_id: "fourier".into(),
                lesson_id: None,
                title: "Questions for Friday".into(),
                body_md: "- Why doesn't the Gibbs overshoot shrink?\n- Lanczos σ-factors?".into(),
                quote: None,
                created_at: ts(20),
                updated_at: ts(20),
            },
        ],
        videos: vec![Video {
            id: "v2".into(),
            lesson_id: "l2".into(),
            status: VideoStatus::Rendering {
                progress: Some(0.62),
            },
            scene_code: Some("class Coefficients(Scene):\n    def construct(self):\n        self.say(\"Orthogonality\")\n".into()),
            log: "Rendering Coefficients\nAnimation 12/20\n".into(),
            duration_seconds: None,
            media: None,
            created_at: ts(0),
            updated_at: ts(0),
        }],
        sources: vec![
            Source {
                media_url: None,
                id: "s1".into(),
                topic_id: Some("fourier".into()),
                name: "Oppenheim ch3.pdf".into(),
                folder: None,
                kind: SourceKind::Pdf,
                bytes: 2_400_000,
                status: SourceStatus::Ready {
                    chunks: 48,
                    pages: Some(32),
                },
                created_at: ts(30),
            },
            Source {
                media_url: None,
                id: "s2".into(),
                topic_id: Some("fourier".into()),
                name: "notes/gibbs.md".into(),
                folder: Some("fourier-notes".into()),
                kind: SourceKind::Text,
                bytes: 5_200,
                status: SourceStatus::Ready {
                    chunks: 3,
                    pages: None,
                },
                created_at: ts(31),
            },
            Source {
                media_url: None,
                id: "s3".into(),
                topic_id: Some("fourier".into()),
                name: "plot_series.py".into(),
                folder: Some("fourier-notes".into()),
                kind: SourceKind::Code,
                bytes: 1_900,
                status: SourceStatus::Failed {
                    error: "The file isn't valid UTF-8 text.".into(),
                },
                created_at: ts(32),
            },
        ],
    }
}

fn default_settings() -> Settings {
    Settings {
        openai_base_url: "https://api.openai.com/v1".into(),
        openai_model: "gpt-5".into(),
        has_api_key: true,
        voice: None,
        render_width: 1920,
        render_height: 1080,
        render_fps: 30,
    }
}

impl FakeState {
    pub fn new() -> Self {
        Self {
            device_name: String::new(),
            approve: false,
            auto_approve_after: None,
            deny: None,
            polls: 0,
            token_collected: false,
            has_api_key: true,
            library: sample_library(),
            proposals: HashMap::new(),
            settings: default_settings(),
            devices: vec![
                Device {
                    id: "d1".into(),
                    name: "test-mac".into(),
                    created_at: ts(0),
                    last_seen_at: Some(ts(100)),
                    current: true,
                },
                Device {
                    id: "d2".into(),
                    name: "iPhone".into(),
                    created_at: ts(5),
                    last_seen_at: None,
                    current: false,
                },
            ],
            next_id: 1,
            simulate: false,
            public_url: String::new(),
            media_dir: None,
            embedding: EmbeddingModelState::NotDownloaded {
                download_bytes: EMBEDDING_DOWNLOAD_BYTES,
            },
        }
    }

    fn embedding_model(&self) -> EmbeddingModel {
        EmbeddingModel {
            repo: EMBEDDING_REPO.into(),
            state: self.embedding.clone(),
        }
    }

    fn id(&mut self, prefix: &str) -> Id {
        self.next_id += 1;
        format!("{prefix}-{}", self.next_id)
    }
}

impl Default for FakeState {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeService {
    pub async fn start() -> Self {
        Self::start_with(FakeState::new(), "127.0.0.1:0").await
    }

    pub async fn start_with(mut state: FakeState, bind: &str) -> Self {
        let listener = tokio::net::TcpListener::bind(bind).await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        state.public_url = url.clone();
        let (events, _) = broadcast::channel(1024);
        let fake = Fake {
            state: Arc::new(Mutex::new(state)),
            events: events.clone(),
        };
        let app = router(fake.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            state: fake.state,
            events,
        }
    }
}

pub fn router(fake: Fake) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/auth/start", post(auth_start))
        .route("/auth/poll", post(auth_poll))
        .route(
            "/auth/fake-authorize",
            get(|| async {
                axum::response::Html(
                    "<p>Fake Margin service: sign-in approved. Return to the app.</p>",
                )
            }),
        )
        .route("/api/me", get(me))
        .route("/api/library", get(library))
        .route("/api/topics", post(create_topic))
        .route(
            "/api/topics/{id}",
            axum::routing::patch(patch_topic).delete(delete_topic),
        )
        .route(
            "/api/lessons/{id}",
            axum::routing::patch(patch_lesson).delete(delete_lesson),
        )
        .route("/api/lessons/{id}/regenerate", post(regenerate))
        .route("/api/lessons/{id}/video", post(make_video))
        .route("/api/notes", post(create_note))
        .route(
            "/api/notes/{id}",
            axum::routing::patch(patch_note).delete(delete_note),
        )
        .route("/api/ask", post(ask))
        .route("/api/proposals/{id}", get(get_proposal))
        .route("/api/proposals/{id}/accept", post(accept))
        .route("/api/videos/{id}", get(get_video))
        .route("/api/search", get(search))
        .route("/api/settings", get(get_settings).patch(patch_settings))
        .route("/api/tools", get(tools))
        .route("/api/devices", get(devices))
        .route("/api/devices/{id}", axum::routing::delete(revoke_device))
        .route(
            "/api/sources",
            post(upload_source).layer(DefaultBodyLimit::max(routes::MAX_SOURCE_BYTES as usize)),
        )
        .route("/api/sources/{id}", axum::routing::delete(delete_source))
        .route(
            "/api/embedding",
            get(get_embedding).delete(delete_embedding),
        )
        .route("/api/embedding/download", post(download_embedding))
        .route("/api/events", get(events))
        .route("/media/videos/{id}/{file}", get(media))
        .with_state(fake)
}

fn api_error(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        Json(ApiError {
            code: code.into(),
            message: message.into(),
        }),
    )
        .into_response()
}

#[allow(clippy::result_large_err)]
fn authorized(headers: &HeaderMap) -> Result<(), Response> {
    let ok = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v == format!("Bearer {TOKEN}"));
    if ok {
        Ok(())
    } else {
        Err(api_error(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Sign in again.",
        ))
    }
}

fn not_found() -> Response {
    api_error(StatusCode::NOT_FOUND, "not_found", "Not found.")
}

fn publish(fake: &Fake, event: Event) {
    let _ = fake.events.send(event);
}

async fn auth_start(State(fake): State<Fake>, Json(req): Json<StartLoginRequest>) -> Response {
    let mut s = fake.state.lock().unwrap();
    s.device_name = req.device_name;
    s.polls = 0;
    s.token_collected = false;
    // Manual runs open a local page instead of GitHub.
    let authorize_url = if s.simulate {
        format!("{}/auth/fake-authorize", s.public_url)
    } else {
        "https://github.com/login/oauth/authorize?client_id=fake&state=login-1".into()
    };
    Json(StartLogin {
        login_id: "login-1".into(),
        poll_secret: "secret".into(),
        authorize_url,
        expires_at: Utc::now() + chrono::Duration::minutes(10),
    })
    .into_response()
}

async fn auth_poll(State(fake): State<Fake>, Json(req): Json<PollLoginRequest>) -> Response {
    let mut s = fake.state.lock().unwrap();
    if req.login_id != "login-1" || req.poll_secret != "secret" || s.token_collected {
        return Json(LoginState::Expired).into_response();
    }
    s.polls += 1;
    if let Some(reason) = s.deny.clone() {
        return Json(LoginState::Denied { reason }).into_response();
    }
    let auto = s.auto_approve_after.is_some_and(|n| s.polls > n);
    if s.approve || auto {
        s.token_collected = true;
        return Json(LoginState::Approved {
            token: TOKEN.into(),
            github_login: "Nkr1shna".into(),
        })
        .into_response();
    }
    Json(LoginState::Pending).into_response()
}

async fn me(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    let device = s.devices.iter().find(|d| d.current).cloned();
    match device {
        Some(device) => Json(Me {
            github_login: "Nkr1shna".into(),
            device,
        })
        .into_response(),
        None => api_error(StatusCode::UNAUTHORIZED, "unauthorized", "Sign in again."),
    }
}

fn with_media(s: &FakeState, mut video: Video) -> Video {
    if video.status == VideoStatus::Ready {
        video.media = Some(fake_media(s, &video.id));
    }
    video
}

/// Media URLs for a ready video. The poster is offered only when `media_dir` has a
/// `poster.png` (like videos rendered before posters existed, there is none otherwise).
fn fake_media(s: &FakeState, video_id: &Id) -> VideoMedia {
    let base = format!("{}/media/videos/{}", s.public_url, video_id);
    let has_poster = s
        .media_dir
        .as_ref()
        .is_some_and(|d| d.join("poster.png").is_file());
    VideoMedia {
        mp4_url: format!("{base}/video.mp4?exp=1&sig=x"),
        storyboard_url: format!("{base}/storyboard.png?exp=1&sig=x"),
        captions_url: Some(format!("{base}/captions.srt?exp=1&sig=x")),
        poster_url: has_poster.then(|| format!("{base}/poster.png?exp=1&sig=x")),
        expires_at: Utc::now() + chrono::Duration::hours(6),
    }
}

async fn library(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    let mut lib = s.library.clone();
    lib.videos = lib.videos.into_iter().map(|v| with_media(&s, v)).collect();
    Json(lib).into_response()
}

async fn create_topic(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<NewTopic>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let topic = {
        let mut s = fake.state.lock().unwrap();
        let id = s.id("topic");
        let position = s
            .library
            .topics
            .iter()
            .filter(|t| t.parent_id == req.parent_id)
            .count() as i64;
        let topic = Topic {
            id,
            parent_id: req.parent_id,
            title: req.title,
            position,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        s.library.topics.push(topic.clone());
        topic
    };
    publish(
        &fake,
        Event::TopicChanged {
            topic: topic.clone(),
        },
    );
    Json(topic).into_response()
}

async fn patch_topic(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<TopicPatch>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let topic = {
        let mut s = fake.state.lock().unwrap();
        let Some(topic) = s.library.topics.iter_mut().find(|t| t.id == id) else {
            return not_found();
        };
        if let Some(title) = req.title {
            topic.title = title;
        }
        if let Some(parent) = req.parent_id {
            topic.parent_id = parent;
        }
        if let Some(position) = req.position {
            topic.position = position;
        }
        topic.updated_at = Utc::now();
        topic.clone()
    };
    publish(
        &fake,
        Event::TopicChanged {
            topic: topic.clone(),
        },
    );
    Json(topic).into_response()
}

async fn delete_topic(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    {
        let mut s = fake.state.lock().unwrap();
        let before = s.library.topics.len();
        s.library.topics.retain(|t| t.id != id);
        if s.library.topics.len() == before {
            return not_found();
        }
        s.library.lessons.retain(|l| l.topic_id != id);
        s.library.notes.retain(|n| n.topic_id != id);
        s.library
            .sources
            .retain(|src| src.topic_id.as_deref() != Some(&id));
    }
    publish(&fake, Event::TopicDeleted { id });
    StatusCode::NO_CONTENT.into_response()
}

async fn patch_lesson(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<LessonPatch>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let lesson = {
        let mut s = fake.state.lock().unwrap();
        let Some(lesson) = s.library.lessons.iter_mut().find(|l| l.id == id) else {
            return not_found();
        };
        if let Some(title) = req.title {
            lesson.title = title;
        }
        if let Some(body) = req.body_md {
            lesson.body_md = body;
        }
        if let Some(position) = req.position {
            lesson.position = position;
        }
        lesson.clone()
    };
    publish(
        &fake,
        Event::LessonChanged {
            lesson: lesson.clone(),
        },
    );
    Json(lesson).into_response()
}

async fn delete_lesson(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    {
        let mut s = fake.state.lock().unwrap();
        let before = s.library.lessons.len();
        s.library.lessons.retain(|l| l.id != id);
        if s.library.lessons.len() == before {
            return not_found();
        }
        s.library
            .notes
            .retain(|n| n.lesson_id.as_deref() != Some(&id));
        s.library.videos.retain(|v| v.lesson_id != id);
    }
    publish(&fake, Event::LessonDeleted { id });
    StatusCode::NO_CONTENT.into_response()
}

async fn regenerate(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (lesson, simulate) = {
        let mut s = fake.state.lock().unwrap();
        let simulate = s.simulate;
        let Some(lesson) = s.library.lessons.iter_mut().find(|l| l.id == id) else {
            return not_found();
        };
        lesson.status = LessonStatus::Queued;
        lesson.body_md.clear();
        (lesson.clone(), simulate)
    };
    publish(
        &fake,
        Event::LessonChanged {
            lesson: lesson.clone(),
        },
    );
    if simulate {
        tokio::spawn(simulate_lesson(fake.clone(), lesson.id.clone()));
    }
    Json(lesson).into_response()
}

async fn simulate_lesson(fake: Fake, id: Id) {
    let words: Vec<String> = LESSON_ONE
        .split_inclusive(' ')
        .map(str::to_string)
        .collect();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let lesson = {
        let mut s = fake.state.lock().unwrap();
        let Some(l) = s.library.lessons.iter_mut().find(|l| l.id == id) else {
            return;
        };
        l.status = LessonStatus::Generating;
        l.clone()
    };
    publish(&fake, Event::LessonChanged { lesson });
    for chunk in words.chunks(6) {
        let text: String = chunk.concat();
        {
            let mut s = fake.state.lock().unwrap();
            let Some(l) = s.library.lessons.iter_mut().find(|l| l.id == id) else {
                return;
            };
            l.body_md.push_str(&text);
        }
        publish(
            &fake,
            Event::LessonDelta {
                lesson_id: id.clone(),
                text,
            },
        );
        tokio::time::sleep(Duration::from_millis(60)).await;
    }
    let lesson = {
        let mut s = fake.state.lock().unwrap();
        let Some(l) = s.library.lessons.iter_mut().find(|l| l.id == id) else {
            return;
        };
        l.status = LessonStatus::Ready;
        l.clone()
    };
    publish(&fake, Event::LessonChanged { lesson });
}

async fn make_video(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(lesson_id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (video, simulate) = {
        let mut s = fake.state.lock().unwrap();
        if !s.library.lessons.iter().any(|l| l.id == lesson_id) {
            return not_found();
        }
        let existing = s
            .library
            .videos
            .iter()
            .find(|v| v.lesson_id == lesson_id)
            .map(|v| v.id.clone());
        let id = match existing {
            Some(id) => id,
            None => s.id("video"),
        };
        s.library.videos.retain(|v| v.lesson_id != lesson_id);
        let video = Video {
            id,
            lesson_id: lesson_id.clone(),
            status: VideoStatus::Queued,
            scene_code: None,
            log: String::new(),
            duration_seconds: None,
            media: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        s.library.videos.push(video.clone());
        (video, s.simulate)
    };
    publish(
        &fake,
        Event::VideoChanged {
            video: video.clone(),
        },
    );
    if simulate {
        tokio::spawn(simulate_video(fake.clone(), video.id.clone()));
    }
    Json(video).into_response()
}

async fn simulate_video(fake: Fake, id: Id) {
    let step = |status: VideoStatus, log: &str, code: Option<&str>| {
        let mut s = fake.state.lock().unwrap();
        let media = fake_media(&s, &id);
        let v = s.library.videos.iter_mut().find(|v| v.id == id)?;
        v.status = status;
        v.log.push_str(log);
        v.log.push('\n');
        if let Some(code) = code {
            v.scene_code = Some(code.to_string());
        }
        let mut video = v.clone();
        if video.status == VideoStatus::Ready {
            video.duration_seconds = Some(12.0);
            v.duration_seconds = Some(12.0);
            video.media = Some(media);
        }
        Some(video)
    };
    let code = "from manim import *\n\nclass WhySines(Scene):\n    def construct(self):\n        title = Text(\"Why sums of sines?\")\n        self.say(\"Every periodic signal hides simple waves.\")\n        self.play(Write(title))\n";
    let mut steps = vec![
        (VideoStatus::Scripting, "Writing the scene", None),
        (
            VideoStatus::Voicing { done: 0, total: 3 },
            "Voicing narration",
            Some(code),
        ),
        (
            VideoStatus::Voicing { done: 1, total: 3 },
            "Line 1 voiced",
            None,
        ),
        (
            VideoStatus::Voicing { done: 2, total: 3 },
            "Line 2 voiced",
            None,
        ),
        (
            VideoStatus::Voicing { done: 3, total: 3 },
            "Line 3 voiced",
            None,
        ),
    ];
    for p in [0.1f32, 0.35, 0.6, 0.85] {
        steps.push((
            VideoStatus::Rendering { progress: Some(p) },
            "Animation progress",
            None,
        ));
    }
    steps.push((VideoStatus::Ready, "Done", None));
    for (status, log, code) in steps {
        tokio::time::sleep(Duration::from_millis(900)).await;
        match step(status, log, code) {
            Some(video) => publish(&fake, Event::VideoChanged { video }),
            None => return,
        }
    }
}

async fn get_video(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    match s.library.videos.iter().find(|v| v.id == id) {
        Some(v) => Json(with_media(&s, v.clone())).into_response(),
        None => not_found(),
    }
}

async fn create_note(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<NewNote>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let note = {
        let mut s = fake.state.lock().unwrap();
        let note = Note {
            id: s.id("note"),
            topic_id: req.topic_id,
            lesson_id: req.lesson_id,
            title: req.title,
            body_md: req.body_md,
            quote: req.quote,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        s.library.notes.push(note.clone());
        note
    };
    publish(&fake, Event::NoteChanged { note: note.clone() });
    Json(note).into_response()
}

async fn patch_note(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<NotePatch>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let note = {
        let mut s = fake.state.lock().unwrap();
        let Some(note) = s.library.notes.iter_mut().find(|n| n.id == id) else {
            return not_found();
        };
        if let Some(title) = req.title {
            note.title = title;
        }
        if let Some(body) = req.body_md {
            note.body_md = body;
        }
        note.updated_at = Utc::now();
        note.clone()
    };
    publish(&fake, Event::NoteChanged { note: note.clone() });
    Json(note).into_response()
}

async fn delete_note(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    {
        let mut s = fake.state.lock().unwrap();
        let before = s.library.notes.len();
        s.library.notes.retain(|n| n.id != id);
        if s.library.notes.len() == before {
            return not_found();
        }
    }
    publish(&fake, Event::NoteDeleted { id });
    StatusCode::NO_CONTENT.into_response()
}

fn outline_for(req: &AskRequest) -> Outline {
    let topic_title = req
        .prompt
        .split([',', '.'])
        .next()
        .unwrap_or("New topic")
        .trim()
        .to_string();
    Outline {
        topic_title: topic_title.clone(),
        subject_title: "Signals & Systems".into(),
        lessons: (1..=req.lesson_count)
            .map(|n| OutlineLesson {
                title: format!("{topic_title}, part {n}"),
                summary: format!("Part {n} of {}", req.lesson_count),
                minutes: 8 + n,
            })
            .collect(),
    }
}

async fn ask(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<AskRequest>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (proposal, simulate) = {
        let mut s = fake.state.lock().unwrap();
        if !s.has_api_key {
            return api_error(
                StatusCode::BAD_REQUEST,
                "no_api_key",
                "Add an OpenAI API key in Settings first.",
            );
        }
        let proposal = Proposal {
            id: s.id("proposal"),
            request: req,
            status: ProposalStatus::Drafting,
            outline: None,
            created_at: Utc::now(),
        };
        s.proposals.insert(proposal.id.clone(), proposal.clone());
        (proposal, s.simulate)
    };
    publish(
        &fake,
        Event::ProposalChanged {
            proposal: proposal.clone(),
        },
    );
    if simulate {
        let fake = fake.clone();
        let id = proposal.id.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            let ready = {
                let mut s = fake.state.lock().unwrap();
                let Some(p) = s.proposals.get_mut(&id) else {
                    return;
                };
                p.outline = Some(outline_for(&p.request));
                p.status = ProposalStatus::Ready;
                p.clone()
            };
            publish(&fake, Event::ProposalChanged { proposal: ready });
        });
    }
    Json(proposal).into_response()
}

async fn get_proposal(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let mut s = fake.state.lock().unwrap();
    let simulate = s.simulate;
    let Some(p) = s.proposals.get_mut(&id) else {
        return not_found();
    };
    if !simulate && p.status == ProposalStatus::Drafting {
        p.outline = Some(outline_for(&p.request));
        p.status = ProposalStatus::Ready;
    }
    Json(p.clone()).into_response()
}

async fn accept(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<AcceptProposal>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (topic, created, simulate, proposal) = {
        let mut s = fake.state.lock().unwrap();
        let Some(p) = s.proposals.get(&id).cloned() else {
            return not_found();
        };
        let parent = match p.request.parent_topic_id.clone() {
            Some(parent) => parent,
            None => {
                let existing = s
                    .library
                    .topics
                    .iter()
                    .find(|t| {
                        t.parent_id.is_none()
                            && t.title.eq_ignore_ascii_case(&req.outline.subject_title)
                    })
                    .map(|t| t.id.clone());
                match existing {
                    Some(id) => id,
                    None => {
                        let subject = Topic {
                            id: s.id("topic"),
                            parent_id: None,
                            title: req.outline.subject_title.clone(),
                            position: 99,
                            created_at: Utc::now(),
                            updated_at: Utc::now(),
                        };
                        let sid = subject.id.clone();
                        s.library.topics.push(subject);
                        sid
                    }
                }
            }
        };
        let topic = Topic {
            id: s.id("topic"),
            parent_id: Some(parent),
            title: req.outline.topic_title.clone(),
            position: 99,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        };
        s.library.topics.push(topic.clone());
        let mut created = Vec::new();
        for (i, l) in req.outline.lessons.iter().enumerate() {
            let lesson = Lesson {
                citations: Vec::new(),
                id: s.id("lesson"),
                topic_id: topic.id.clone(),
                title: l.title.clone(),
                summary: l.summary.clone(),
                position: i as i64,
                level: p.request.level,
                minutes: l.minutes,
                body_md: String::new(),
                status: LessonStatus::Queued,
                created_at: Utc::now(),
                updated_at: Utc::now(),
            };
            s.library.lessons.push(lesson.clone());
            created.push(lesson);
        }
        // The proposal's sources now belong to the new topic.
        for src in s.library.sources.iter_mut() {
            if src.topic_id.is_none() && p.request.source_ids.contains(&src.id) {
                src.topic_id = Some(topic.id.clone());
            }
        }
        let proposal = {
            let p = s.proposals.get_mut(&id).unwrap();
            p.status = ProposalStatus::Accepted {
                topic_id: topic.id.clone(),
            };
            p.outline = Some(req.outline.clone());
            p.clone()
        };
        (topic, created, s.simulate, proposal)
    };
    let topics: Vec<Topic> = fake.state.lock().unwrap().library.topics.clone();
    for t in topics {
        publish(&fake, Event::TopicChanged { topic: t });
    }
    for lesson in &created {
        publish(
            &fake,
            Event::LessonChanged {
                lesson: lesson.clone(),
            },
        );
    }
    let attached: Vec<Source> = {
        let s = fake.state.lock().unwrap();
        s.library
            .sources
            .iter()
            .filter(|src| src.topic_id.as_deref() == Some(topic.id.as_str()))
            .cloned()
            .collect()
    };
    for source in attached {
        publish(&fake, Event::SourceChanged { source });
    }
    publish(&fake, Event::ProposalChanged { proposal });
    if simulate {
        for lesson in created {
            tokio::spawn(simulate_lesson(fake.clone(), lesson.id));
        }
    }
    Json(topic).into_response()
}

#[derive(serde::Deserialize)]
struct SearchQuery {
    q: String,
}

async fn search(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Query(query): Query<SearchQuery>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    let q = query.q.to_lowercase();
    let mut hits = Vec::new();
    let snippet = |text: &str| -> String {
        let lower = text.to_lowercase();
        match lower.find(&q) {
            Some(i) => {
                let start = text[..i]
                    .char_indices()
                    .rev()
                    .nth(40)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                text[start..].chars().take(160).collect()
            }
            None => text.chars().take(160).collect(),
        }
    };
    let crumb = |topic_id: &str| -> String {
        let mut parts = Vec::new();
        let mut cur = s.library.topics.iter().find(|t| t.id == topic_id);
        while let Some(t) = cur {
            parts.push(t.title.clone());
            cur = t
                .parent_id
                .as_ref()
                .and_then(|p| s.library.topics.iter().find(|x| &x.id == p));
        }
        parts.reverse();
        parts.join(" / ")
    };
    for t in &s.library.topics {
        if t.title.to_lowercase().contains(&q) {
            hits.push(SearchHit {
                kind: SearchKind::Topic,
                id: t.id.clone(),
                title: t.title.clone(),
                context: crumb(&t.id),
                snippet: String::new(),
            });
        }
    }
    for l in &s.library.lessons {
        if l.title.to_lowercase().contains(&q) || l.body_md.to_lowercase().contains(&q) {
            hits.push(SearchHit {
                kind: SearchKind::Lesson,
                id: l.id.clone(),
                title: l.title.clone(),
                context: crumb(&l.topic_id),
                snippet: snippet(&l.body_md),
            });
        }
    }
    for n in &s.library.notes {
        if n.title.to_lowercase().contains(&q) || n.body_md.to_lowercase().contains(&q) {
            hits.push(SearchHit {
                kind: SearchKind::Note,
                id: n.id.clone(),
                title: n.title.clone(),
                context: crumb(&n.topic_id),
                snippet: snippet(&n.body_md),
            });
        }
    }
    Json(hits).into_response()
}

async fn get_settings(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    let mut settings = s.settings.clone();
    settings.has_api_key = s.has_api_key;
    Json(settings).into_response()
}

async fn patch_settings(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<SettingsPatch>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let settings = {
        let mut s = fake.state.lock().unwrap();
        if let Some(v) = req.openai_base_url {
            s.settings.openai_base_url = v;
        }
        if let Some(v) = req.openai_model {
            s.settings.openai_model = v;
        }
        if let Some(key) = req.openai_api_key {
            s.has_api_key = !key.is_empty();
        }
        if let Some(v) = req.voice {
            s.settings.voice = v;
        }
        if let Some(v) = req.render_width {
            s.settings.render_width = v;
        }
        if let Some(v) = req.render_height {
            s.settings.render_height = v;
        }
        if let Some(v) = req.render_fps {
            s.settings.render_fps = v;
        }
        s.settings.has_api_key = s.has_api_key;
        s.settings.clone()
    };
    publish(
        &fake,
        Event::SettingsChanged {
            settings: settings.clone(),
        },
    );
    Json(settings).into_response()
}

async fn tools(headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    Json(vec![
        ToolStatus {
            name: "manimgx".into(),
            found: true,
            path: Some("/Users/me/.local/bin/manimgx".into()),
            version: Some("0.1.1".into()),
            install_commands: vec!["uv tool install manimgx".into()],
        },
        ToolStatus {
            name: "fluidaudiocli".into(),
            found: false,
            path: None,
            version: None,
            install_commands: vec![
                "git clone https://github.com/FluidInference/FluidAudio.git".into(),
                "cd FluidAudio && swift build -c release".into(),
            ],
        },
    ])
    .into_response()
}

async fn devices(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    Json(fake.state.lock().unwrap().devices.clone()).into_response()
}

async fn revoke_device(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let mut s = fake.state.lock().unwrap();
    let before = s.devices.len();
    s.devices.retain(|d| d.id != id);
    if s.devices.len() == before {
        return not_found();
    }
    StatusCode::NO_CONTENT.into_response()
}

#[derive(serde::Deserialize)]
struct UploadQuery {
    name: String,
    folder: Option<String>,
    topic_id: Option<String>,
}

async fn upload_source(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Query(query): Query<UploadQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let Some(kind) = SourceKind::for_name(&query.name) else {
        return api_error(
            StatusCode::BAD_REQUEST,
            "unsupported_source",
            "Margin can't read this kind of file.",
        );
    };
    let (source, process) = {
        let mut s = fake.state.lock().unwrap();
        if let Some(topic_id) = &query.topic_id
            && !s.library.topics.iter().any(|t| &t.id == topic_id)
        {
            return not_found();
        }
        let source = Source {
            media_url: None,
            id: s.id("source"),
            topic_id: query.topic_id,
            name: query.name,
            folder: query.folder,
            kind,
            bytes: body.len() as u64,
            status: SourceStatus::Queued,
            created_at: Utc::now(),
        };
        s.library.sources.push(source.clone());
        let ready = matches!(s.embedding, EmbeddingModelState::Ready { .. });
        (source, s.simulate && ready)
    };
    publish(
        &fake,
        Event::SourceChanged {
            source: source.clone(),
        },
    );
    if process {
        tokio::spawn(simulate_source(fake.clone(), source.id.clone()));
    }
    Json(source).into_response()
}

/// Read and embed a source over a couple of seconds, a chunk at a time.
async fn simulate_source(fake: Fake, id: Id) {
    let set = |status: SourceStatus| {
        let mut s = fake.state.lock().unwrap();
        let src = s.library.sources.iter_mut().find(|src| src.id == id)?;
        src.status = status;
        Some(src.clone())
    };
    let total = 4;
    for done in 0..=total {
        tokio::time::sleep(Duration::from_millis(400)).await;
        match set(SourceStatus::Processing { done, total }) {
            Some(source) => publish(&fake, Event::SourceChanged { source }),
            None => return,
        }
    }
    let pdf = fake
        .state
        .lock()
        .unwrap()
        .library
        .sources
        .iter()
        .any(|src| src.id == id && src.kind == SourceKind::Pdf);
    let status = SourceStatus::Ready {
        chunks: total,
        pages: pdf.then_some(2),
    };
    if let Some(source) = set(status) {
        publish(&fake, Event::SourceChanged { source });
    }
}

async fn delete_source(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    {
        let mut s = fake.state.lock().unwrap();
        let before = s.library.sources.len();
        s.library.sources.retain(|src| src.id != id);
        if s.library.sources.len() == before {
            return not_found();
        }
    }
    publish(&fake, Event::SourceDeleted { id });
    StatusCode::NO_CONTENT.into_response()
}

async fn get_embedding(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    Json(fake.state.lock().unwrap().embedding_model()).into_response()
}

fn set_embedding(fake: &Fake, state: EmbeddingModelState) -> EmbeddingModel {
    let model = {
        let mut s = fake.state.lock().unwrap();
        s.embedding = state;
        s.embedding_model()
    };
    publish(
        fake,
        Event::EmbeddingModelChanged {
            model: model.clone(),
        },
    );
    model
}

async fn download_embedding(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (current, simulate) = {
        let s = fake.state.lock().unwrap();
        (s.embedding_model(), s.simulate)
    };
    // Already downloading or downloaded: nothing to start.
    if matches!(
        current.state,
        EmbeddingModelState::Downloading { .. } | EmbeddingModelState::Ready { .. }
    ) {
        return Json(current).into_response();
    }
    let model = set_embedding(
        &fake,
        EmbeddingModelState::Downloading {
            received: 0,
            total: EMBEDDING_DOWNLOAD_BYTES,
        },
    );
    if simulate {
        tokio::spawn(simulate_download(fake.clone()));
    }
    Json(model).into_response()
}

/// Download over ~5 s, then read the sources that were waiting for the model.
async fn simulate_download(fake: Fake) {
    let total = EMBEDDING_DOWNLOAD_BYTES;
    for step in 1..=10u64 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        // Removed while downloading: stop.
        let downloading = matches!(
            fake.state.lock().unwrap().embedding,
            EmbeddingModelState::Downloading { .. }
        );
        if !downloading {
            return;
        }
        let state = if step == 10 {
            EmbeddingModelState::Ready { bytes: total }
        } else {
            EmbeddingModelState::Downloading {
                received: total * step / 10,
                total,
            }
        };
        set_embedding(&fake, state);
    }
    let waiting: Vec<Id> = fake
        .state
        .lock()
        .unwrap()
        .library
        .sources
        .iter()
        .filter(|src| src.status == SourceStatus::Queued)
        .map(|src| src.id.clone())
        .collect();
    for id in waiting {
        simulate_source(fake.clone(), id).await;
    }
}

async fn delete_embedding(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let model = set_embedding(
        &fake,
        EmbeddingModelState::NotDownloaded {
            download_bytes: EMBEDDING_DOWNLOAD_BYTES,
        },
    );
    Json(model).into_response()
}

async fn events(State(fake): State<Fake>, headers: HeaderMap, ws: WebSocketUpgrade) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let rx = fake.events.subscribe();
    ws.on_upgrade(move |socket| socket_loop(socket, rx))
}

async fn socket_loop(mut socket: WebSocket, mut rx: broadcast::Receiver<Event>) {
    let hello = Event::Hello {
        server_version: "fake-0.1".into(),
    };
    let json = serde_json::to_string(&hello).unwrap();
    if socket.send(Message::Text(json.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            event = rx.recv() => {
                match event {
                    Ok(event) => {
                        let json = serde_json::to_string(&event).unwrap();
                        if socket.send(Message::Text(json.into())).await.is_err() {
                            return;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => return,
                }
            }
            incoming = socket.recv() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None | Some(Err(_)) => return,
                    _ => {}
                }
            }
        }
    }
}

async fn media(State(fake): State<Fake>, Path((id, file)): Path<(String, String)>) -> Response {
    let dir = fake.state.lock().unwrap().media_dir.clone();
    if file == "captions.srt" {
        return (
            [(header::CONTENT_TYPE, "application/x-subrip")],
            "1\n00:00:00,500 --> 00:00:04,000\nEvery periodic signal hides simple waves.\n\n2\n00:00:04,500 --> 00:00:09,000\nLet's find them.\n",
        )
            .into_response();
    }
    let Some(dir) = dir else {
        return not_found();
    };
    let _ = id;
    match std::fs::read(dir.join(&file)) {
        Ok(bytes) => {
            let mime = if file.ends_with(".mp4") {
                "video/mp4"
            } else if file.ends_with(".png") {
                "image/png"
            } else {
                "application/octet-stream"
            };
            ([(header::CONTENT_TYPE, mime)], bytes).into_response()
        }
        Err(_) => not_found(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn poster_is_offered_only_when_the_media_dir_has_one() {
        let dir = std::env::temp_dir().join(format!("margin-fake-poster-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut state = FakeState::new();
        state.public_url = "http://fake".into();
        let id: Id = "video-1".into();
        assert_eq!(fake_media(&state, &id).poster_url, None);
        state.media_dir = Some(dir.clone());
        assert_eq!(fake_media(&state, &id).poster_url, None);
        std::fs::write(dir.join("poster.png"), b"png").unwrap();
        assert_eq!(
            fake_media(&state, &id).poster_url.as_deref(),
            Some("http://fake/media/videos/video-1/poster.png?exp=1&sig=x")
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
