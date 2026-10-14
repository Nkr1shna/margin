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
    /// Uploaded sources' bytes, served as their originals.
    pub source_files: HashMap<Id, Vec<u8>>,
    /// Uploads in parts (`routes::SOURCE_UPLOADS`) still arriving.
    pub part_uploads: HashMap<Id, PartUpload>,
    /// How many parts have arrived, for tests.
    pub parts_received: u32,
    /// Fail the next this many parts with a 503, for tests of the retries.
    pub fail_parts: u32,
    pub chats: Vec<Chat>,
    /// Each chat's transcript, in `seq` order.
    pub chat_items: HashMap<Id, Vec<ChatItem>>,
    /// Full outputs of tool results, by item id.
    pub chat_outputs: HashMap<Id, String>,
    /// The last `ChatItem.seq` given out.
    pub chat_seq: u64,
    /// Turns stop part-way through their answer and stay running (to test Stop and the
    /// pending message).
    pub chat_hang: bool,
    pub workspaces: Vec<Workspace>,
}

/// An upload in parts the fake is receiving.
#[derive(Debug, Clone)]
pub struct PartUpload {
    pub name: String,
    pub folder: Option<String>,
    pub topic_id: Option<Id>,
    pub bytes: u64,
    pub data: Vec<u8>,
}

/// Hugging Face repo of the embedding model, as the service reports it.
pub const EMBEDDING_REPO: &str = "FluidInference/embeddinggemma-2-coreml";
/// What the fake says the model download weighs.
pub const EMBEDDING_DOWNLOAD_BYTES: u64 = 1_480_000_000;

#[derive(Clone)]
pub struct Fake {
    pub state: Arc<Mutex<FakeState>>,
    pub events: broadcast::Sender<Event>,
    /// Frames for the device on the socket (every connected socket gets them here).
    pub directed: broadcast::Sender<DirectedFrame>,
}

pub struct FakeService {
    pub url: String,
    pub state: Arc<Mutex<FakeState>>,
    pub events: broadcast::Sender<Event>,
    pub directed: broadcast::Sender<DirectedFrame>,
}

fn ts(n: i64) -> DateTime<Utc> {
    Utc.timestamp_opt(1_760_000_000 + n, 0).unwrap()
}

pub const LESSON_ONE: &str = r#"Any periodic signal that's reasonably well behaved can be rebuilt from sine and cosine waves. Their frequencies are whole-number multiples of one base frequency.[^1]

## The idea

The slowest wave sets the period $T$. The faster ones repeat a whole number of times inside each period, so they add detail without breaking the repetition. That's why the frequencies must be whole-number multiples, and it's what makes the decomposition unique.[^1]

$$f(t) = \frac{a_0}{2} + \sum_{n=1}^{\infty} \left( a_n \cos n\omega_0 t + b_n \sin n\omega_0 t \right), \quad \omega_0 = \frac{2\pi}{T}$$

Each pair $a_n, b_n$ says how much of the $n$-th harmonic the signal contains, so sines & cosines together describe the whole signal.[^2]

## Building a square wave

A square wave of height 1 uses only odd harmonics, and only sines:[^3]

$$f(t) = \frac{4}{\pi}\left[\sin \omega_0 t + \tfrac{1}{3}\sin 3\omega_0 t + \tfrac{1}{5}\sin 5\omega_0 t + \dots\right]$$

With three terms the shape is already recognisable.[^4] With fifty, only a small overshoot at each jump remains. That overshoot, about 9% of the jump, never goes away.[^5][^6]

> **Key idea.** Every term repeats with period $T$ because its frequency is an integer multiple of $\omega_0$. Adding terms moves the ripple closer to the jump without shrinking it.[^7]

## Check yourself

<details><summary>Why does the square wave have no cosine terms?</summary>

Centred at zero, it's an odd function: $f(-t) = -f(t)$. Cosines are even, so their coefficients integrate to zero.

</details>

<details><summary>If you double the period T, what happens to ω₀?</summary>

It halves, since $\omega_0 = 2\pi / T$.

</details>
"#;

/// The text sources' contents, served as their originals.
const GIBBS_MD: &str = "# Gibbs overshoot\n\nNear a jump the partial sums overshoot by about 9% of the jump, \
whatever the number of terms. More terms squeeze the ripple towards the jump; its height stays.\n";
const PARTIAL_SUMS_PY: &str = "import numpy as np\n\n\ndef square_coefficients(n_max):\n    \
\"\"\"b_n of a unit square wave: 4/(n*pi) for odd n, zero for even n.\"\"\"\n    \
n = np.arange(1, n_max + 1)\n    return np.where(n % 2 == 1, 4 / (n * np.pi), 0.0)\n";

/// Where the sample sources' media live (`examples/sample-media`, made by its
/// `generate.swift`).
pub fn sample_media_dir() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/sample-media")
}

/// The sample file served as `/media/sources/{source_id}/{file}`.
pub fn sample_media_file(source_id: &str, file: &str) -> Option<std::path::PathBuf> {
    let name = match (source_id, file) {
        ("s1", "original") => "oppenheim-ch3.pdf",
        ("s1", "page-3.png") => "oppenheim-ch3-page-3.png",
        ("s4", "original") => "square-wave.png",
        ("s5", "original") => "lecture-12.m4a",
        ("s6", "original") => "gibbs-demo.mp4",
        ("s6", "frame-2200.jpg") => "gibbs-demo-frame-2200.jpg",
        _ => return None,
    };
    Some(sample_media_dir().join(name))
}

/// A source's signed media URL as the fake hands it out: relative in the sample library,
/// made absolute (`rebase_media`) when a fake service starts.
fn media_url(source_id: &str, file: &str) -> String {
    format!("/media/sources/{source_id}/{file}?exp=1&sig=x")
}

/// Make the library's relative media URLs (`/media/…`) absolute under `base`.
pub fn rebase_media(library: &mut Library, base: impl Fn(&str) -> String) {
    let fix = |url: &mut Option<String>| {
        if let Some(u) = url.as_mut()
            && u.starts_with("/media/")
        {
            *u = base(u);
        }
    };
    for source in &mut library.sources {
        fix(&mut source.media_url);
    }
    for lesson in &mut library.lessons {
        for c in &mut lesson.citations {
            fix(&mut c.image_url);
        }
    }
}

/// What "Why sums of sines?" cites: one passage of every kind of source.
pub fn sample_citations() -> Vec<Citation> {
    let cite = |n, source_id: &str, source_name: &str, kind| Citation {
        n,
        source_id: source_id.into(),
        source_name: source_name.into(),
        kind,
        page: None,
        start_ms: None,
        end_ms: None,
        excerpt: String::new(),
        image_url: None,
    };
    vec![
        Citation {
            page: Some(3),
            excerpt: "A periodic signal x(t) with fundamental period T can be represented as a linear combination of harmonically related complex exponentials, each with a frequency that is an integer multiple of the fundamental frequency ω₀ = 2π/T.".into(),
            image_url: Some(media_url("s1", "page-3.png")),
            ..cite(1, "s1", "Oppenheim ch3.pdf", SourceKind::Pdf)
        },
        Citation {
            excerpt: PARTIAL_SUMS_PY.trim_end().into(),
            ..cite(2, "s7", "partial_sums.py", SourceKind::Code)
        },
        Citation {
            image_url: Some(media_url("s4", "original")),
            ..cite(3, "s4", "square-wave.png", SourceKind::Image)
        },
        Citation {
            start_ms: Some(2_200),
            end_ms: Some(4_900),
            excerpt: "With three terms the shape is already recognisable.".into(),
            image_url: Some(media_url("s6", "frame-2200.jpg")),
            ..cite(4, "s6", "gibbs-demo.mp4", SourceKind::Video)
        },
        Citation {
            excerpt: "Near a jump the partial sums overshoot by about 9% of the jump, whatever the number of terms.".into(),
            ..cite(5, "s2", "notes/gibbs.md", SourceKind::Text)
        },
        Citation {
            start_ms: Some(3_725),
            end_ms: Some(8_560),
            excerpt: "Near a discontinuity, the partial sums overshoot by about nine percent of the jump.".into(),
            ..cite(6, "s5", "lecture-12.m4a", SourceKind::Audio)
        },
        Citation {
            start_ms: Some(8_960),
            end_ms: Some(13_752),
            excerpt: "Adding more terms squeezes the overshoot closer to the jump, but it never gets smaller.".into(),
            ..cite(7, "s5", "lecture-12.m4a", SourceKind::Audio)
        },
    ]
}

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
            Lesson {
                citations: sample_citations(),
                ..lesson(
                    "l1",
                    "fourier",
                    "Why sums of sines?",
                    0,
                    LessonStatus::Ready,
                    LESSON_ONE,
                )
            },
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
                media_url: Some(media_url("s1", "original")),
                id: "s1".into(),
                topic_id: Some("fourier".into()),
                name: "Oppenheim ch3.pdf".into(),
                folder: None,
                kind: SourceKind::Pdf,
                bytes: 2_400_000,
                status: SourceStatus::Ready {
                    chunks: 48,
                    pages: Some(32),
                    warning: None,
                },
                created_at: ts(30),
            },
            Source {
                media_url: Some(media_url("s2", "original")),
                id: "s2".into(),
                topic_id: Some("fourier".into()),
                name: "notes/gibbs.md".into(),
                folder: Some("fourier-notes".into()),
                kind: SourceKind::Text,
                bytes: 5_200,
                status: SourceStatus::Ready {
                    chunks: 3,
                    pages: None,
                    warning: None,
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
            Source {
                media_url: Some(media_url("s7", "original")),
                id: "s7".into(),
                topic_id: Some("fourier".into()),
                name: "partial_sums.py".into(),
                folder: Some("fourier-notes".into()),
                kind: SourceKind::Code,
                bytes: PARTIAL_SUMS_PY.len() as u64,
                status: SourceStatus::Ready {
                    chunks: 1,
                    pages: None,
                    warning: None,
                },
                created_at: ts(33),
            },
            Source {
                media_url: Some(media_url("s4", "original")),
                id: "s4".into(),
                topic_id: Some("fourier".into()),
                name: "square-wave.png".into(),
                folder: None,
                kind: SourceKind::Image,
                bytes: 53_343,
                status: SourceStatus::Ready {
                    chunks: 1,
                    pages: None,
                    warning: None,
                },
                created_at: ts(34),
            },
            Source {
                media_url: Some(media_url("s5", "original")),
                id: "s5".into(),
                topic_id: Some("fourier".into()),
                name: "lecture-12.m4a".into(),
                folder: None,
                kind: SourceKind::Audio,
                bytes: 70_003,
                status: SourceStatus::Ready {
                    chunks: 4,
                    pages: None,
                    warning: None,
                },
                created_at: ts(35),
            },
            Source {
                media_url: Some(media_url("s6", "original")),
                id: "s6".into(),
                topic_id: Some("fourier".into()),
                name: "gibbs-demo.mp4".into(),
                folder: None,
                kind: SourceKind::Video,
                bytes: 127_994,
                status: SourceStatus::Ready { chunks: 5, pages: None, warning: None, },
                created_at: ts(36),
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
        reasoning_effort: ReasoningEffort::Medium,
        agent_model: None,
    }
}

impl FakeState {
    pub fn new() -> Self {
        let mut state = Self {
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
                    online: true,
                },
                Device {
                    id: "d2".into(),
                    name: "iPhone".into(),
                    created_at: ts(5),
                    last_seen_at: None,
                    current: false,
                    online: false,
                },
            ],
            next_id: 1,
            simulate: false,
            public_url: String::new(),
            media_dir: None,
            embedding: EmbeddingModelState::NotDownloaded {
                download_bytes: EMBEDDING_DOWNLOAD_BYTES,
            },
            source_files: HashMap::new(),
            part_uploads: HashMap::new(),
            parts_received: 0,
            fail_parts: 0,
            chats: Vec::new(),
            chat_items: HashMap::new(),
            chat_outputs: HashMap::new(),
            chat_seq: 100,
            chat_hang: false,
            workspaces: Vec::new(),
        };
        for (chat, items) in sample_chats() {
            for item in &items {
                if let ChatItemKind::ToolResult { call_id, .. } = &item.kind {
                    state
                        .chat_outputs
                        .insert(item.id.clone(), sample_tool_output(call_id));
                }
            }
            state.chat_items.insert(chat.id.clone(), items);
            state.chats.push(chat);
        }
        state
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
        rebase_media(&mut state.library, |path| format!("{url}{path}"));
        for items in state.chat_items.values_mut() {
            rebase_chat_media(items, |path| format!("{url}{path}"));
        }
        let (events, _) = broadcast::channel(1024);
        let (directed, _) = broadcast::channel(64);
        let fake = Fake {
            state: Arc::new(Mutex::new(state)),
            events: events.clone(),
            directed: directed.clone(),
        };
        let app = router(fake.clone());
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        Self {
            url,
            state: fake.state,
            events,
            directed,
        }
    }

    /// Send a frame to the connected devices, as the service sends a device-tool request.
    pub fn direct(&self, frame: DirectedFrame) {
        let _ = self.directed.send(frame);
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
            post(upload_source).layer(DefaultBodyLimit::max(routes::MAX_ANY_SOURCE_BYTES as usize)),
        )
        .route("/api/sources/{id}", axum::routing::delete(delete_source))
        .route("/api/sources/{id}/reprocess", post(reprocess_source))
        .route("/api/sources/{id}/transcript", get(source_transcript))
        .route("/api/source-uploads", post(start_part_upload))
        .route(
            "/api/source-uploads/{id}",
            get(get_part_upload)
                .put(put_part)
                .delete(cancel_part_upload)
                .layer(DefaultBodyLimit::max(routes::UPLOAD_PART_BYTES as usize)),
        )
        .route("/api/source-uploads/{id}/finish", post(finish_part_upload))
        .route(
            "/api/embedding",
            get(get_embedding).delete(delete_embedding),
        )
        .route("/api/embedding/download", post(download_embedding))
        .route("/api/chats", get(list_chats).post(create_chat))
        .route(
            "/api/chats/{id}",
            get(get_chat).patch(patch_chat).delete(delete_chat),
        )
        .route("/api/chats/{id}/messages", post(send_message))
        .route(
            "/api/chats/{id}/pending",
            axum::routing::delete(drop_pending),
        )
        .route("/api/chats/{id}/stop", post(stop_chat))
        .route("/api/chats/{id}/continue", post(continue_chat))
        .route("/api/chats/{id}/confirm", post(confirm))
        .route("/api/chats/{id}/items/{item}/output", get(chat_item_output))
        .route(
            "/api/workspaces",
            get(list_workspaces).post(create_workspace),
        )
        .route(
            "/api/workspaces/{id}",
            axum::routing::delete(delete_workspace),
        )
        .route("/api/events", get(events))
        .route("/media/videos/{id}/{file}", get(media))
        .route("/media/sources/{id}/{file}", get(source_media))
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
        lesson.citations.clear();
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
        // The finished lesson carries what its markers cite, like the service's.
        let mut citations = sample_citations();
        for url in citations.iter_mut().filter_map(|c| c.image_url.as_mut()) {
            *url = format!("{}{url}", s.public_url);
        }
        let Some(l) = s.library.lessons.iter_mut().find(|l| l.id == id) else {
            return;
        };
        l.status = LessonStatus::Ready;
        l.citations = citations;
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
            chat_id: None,
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
        if let Some(v) = req.reasoning_effort {
            s.settings.reasoning_effort = v;
        }
        if let Some(v) = req.agent_model {
            s.settings.agent_model = v.map(|m| m.trim().to_string()).filter(|m| !m.is_empty());
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
    let kind = match check_upload(&fake, &query, body.len() as u64) {
        Ok(kind) => kind,
        Err(r) => return r,
    };
    add_source(&fake, query, kind, body.to_vec())
}

/// The kind of an upload's file, or the service's refusal: a name Margin can't read,
/// a size over its kind's limit, a topic that doesn't exist.
#[allow(clippy::result_large_err)]
fn check_upload(fake: &Fake, query: &UploadQuery, bytes: u64) -> Result<SourceKind, Response> {
    let Some(kind) = SourceKind::for_name(&query.name) else {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "unsupported_source",
            &format!(
                "Margin can't read {}. Sources can be {SOURCE_KINDS} files.",
                query.name
            ),
        ));
    };
    if bytes > routes::max_source_bytes(kind) {
        return Err(api_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "too_large",
            &routes::too_large_text(&query.name, kind),
        ));
    }
    if let Some(topic_id) = &query.topic_id
        && !fake
            .state
            .lock()
            .unwrap()
            .library
            .topics
            .iter()
            .any(|t| &t.id == topic_id)
    {
        return Err(not_found());
    }
    Ok(kind)
}

/// Records an uploaded file as a Queued source (read over a couple of seconds when
/// simulating with the model downloaded).
fn add_source(fake: &Fake, query: UploadQuery, kind: SourceKind, body: Vec<u8>) -> Response {
    let (source, process) = {
        let mut s = fake.state.lock().unwrap();
        let id = s.id("source");
        let source = Source {
            media_url: Some(format!("{}{}", s.public_url, media_url(&id, "original"))),
            id,
            topic_id: query.topic_id,
            name: query.name,
            folder: query.folder,
            kind,
            bytes: body.len() as u64,
            status: SourceStatus::Queued,
            created_at: Utc::now(),
        };
        s.source_files.insert(source.id.clone(), body);
        s.library.sources.push(source.clone());
        let ready = matches!(s.embedding, EmbeddingModelState::Ready { .. });
        (source, s.simulate && ready)
    };
    publish(
        fake,
        Event::SourceChanged {
            source: source.clone(),
        },
    );
    if process {
        tokio::spawn(simulate_source(fake.clone(), source.id.clone()));
    }
    Json(source).into_response()
}

#[derive(serde::Deserialize)]
struct StartPartsQuery {
    name: String,
    folder: Option<String>,
    topic_id: Option<String>,
    bytes: u64,
}

async fn start_part_upload(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Query(q): Query<StartPartsQuery>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let query = UploadQuery {
        name: q.name,
        folder: q.folder,
        topic_id: q.topic_id,
    };
    if let Err(r) = check_upload(&fake, &query, q.bytes) {
        return r;
    }
    let mut s = fake.state.lock().unwrap();
    let id = s.id("upload");
    s.part_uploads.insert(
        id.clone(),
        PartUpload {
            name: query.name,
            folder: query.folder,
            topic_id: query.topic_id,
            bytes: q.bytes,
            data: Vec::new(),
        },
    );
    Json(SourceUpload {
        id,
        bytes: q.bytes,
        received: 0,
    })
    .into_response()
}

async fn get_part_upload(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    match s.part_uploads.get(&id) {
        Some(u) => Json(SourceUpload {
            id,
            bytes: u.bytes,
            received: u.data.len() as u64,
        })
        .into_response(),
        None => not_found(),
    }
}

#[derive(serde::Deserialize)]
struct PartQuery {
    offset: u64,
}

async fn put_part(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<PartQuery>,
    body: Bytes,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let mut s = fake.state.lock().unwrap();
    if s.fail_parts > 0 {
        s.fail_parts -= 1;
        return api_error(StatusCode::SERVICE_UNAVAILABLE, "internal", "Try again.");
    }
    s.parts_received += 1;
    let Some(u) = s.part_uploads.get_mut(&id) else {
        return not_found();
    };
    if q.offset != u.data.len() as u64 {
        return api_error(
            StatusCode::CONFLICT,
            "wrong_offset",
            "The next part starts elsewhere.",
        );
    }
    if u.data.len() as u64 + body.len() as u64 > u.bytes {
        return api_error(StatusCode::PAYLOAD_TOO_LARGE, "too_large", "Too much.");
    }
    u.data.extend_from_slice(&body);
    Json(SourceUpload {
        id,
        bytes: u.bytes,
        received: u.data.len() as u64,
    })
    .into_response()
}

async fn finish_part_upload(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let upload = {
        let mut s = fake.state.lock().unwrap();
        match s.part_uploads.get(&id) {
            None => return not_found(),
            Some(u) if u.data.len() as u64 != u.bytes => {
                return api_error(
                    StatusCode::CONFLICT,
                    "incomplete_upload",
                    "Parts are missing.",
                );
            }
            Some(_) => s.part_uploads.remove(&id).unwrap(),
        }
    };
    let query = UploadQuery {
        name: upload.name,
        folder: upload.folder,
        topic_id: upload.topic_id,
    };
    match check_upload(&fake, &query, upload.bytes) {
        Ok(kind) => add_source(&fake, query, kind, upload.data),
        Err(r) => r,
    }
}

async fn cancel_part_upload(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    match fake.state.lock().unwrap().part_uploads.remove(&id) {
        Some(_) => StatusCode::NO_CONTENT.into_response(),
        None => not_found(),
    }
}

/// "Read again": Queued, then read anew when simulating with the model downloaded; the
/// sample recording read without a transcript comes back with one.
async fn reprocess_source(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (source, process) = {
        let mut s = fake.state.lock().unwrap();
        let ready = matches!(s.embedding, EmbeddingModelState::Ready { .. });
        let simulate = s.simulate;
        let Some(src) = s.library.sources.iter_mut().find(|src| src.id == id) else {
            return not_found();
        };
        if matches!(
            src.status,
            SourceStatus::Queued | SourceStatus::Processing { .. }
        ) {
            return api_error(
                StatusCode::CONFLICT,
                "source_busy",
                &format!("{} is already being read.", src.name),
            );
        }
        src.status = SourceStatus::Queued;
        (src.clone(), simulate && ready)
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

async fn source_transcript(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    if !s.library.sources.iter().any(|src| src.id == id) {
        return not_found();
    }
    Json(sample_transcript(&id)).into_response()
}

/// What the sample recordings say, line by line (the sentence times printed by
/// `examples/sample-media/generate.swift`).
pub fn sample_transcript(source_id: &str) -> Vec<TranscriptSegment> {
    let line = |start_ms, end_ms, text: &str| TranscriptSegment {
        start_ms,
        end_ms,
        text: text.into(),
    };
    match source_id {
        "s5" => vec![
            line(0, 1_050, "Lecture twelve."),
            line(1_200, 3_325, "Today we look at what happens near a jump."),
            line(
                3_725,
                8_560,
                "Near a discontinuity, the partial sums overshoot by about nine percent of the jump.",
            ),
            line(
                8_960,
                13_752,
                "Adding more terms squeezes the overshoot closer to the jump, but it never gets smaller.",
            ),
            line(
                14_152,
                16_420,
                "That stubborn ripple is the Gibbs phenomenon.",
            ),
        ],
        "s6" => vec![
            line(300, 2_000, "With one term it's a sine wave."),
            line(
                2_200,
                4_900,
                "With three terms the shape is already recognisable.",
            ),
            line(
                5_100,
                8_350,
                "With fifteen, only the overshoot at each jump remains.",
            ),
        ],
        _ => Vec::new(),
    }
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
        warning: None,
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
    let directed = fake.directed.subscribe();
    ws.on_upgrade(move |socket| socket_loop(socket, rx, directed))
}

async fn socket_loop(
    mut socket: WebSocket,
    mut rx: broadcast::Receiver<Event>,
    mut directed: broadcast::Receiver<DirectedFrame>,
) {
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
            frame = directed.recv() => {
                match frame {
                    Ok(frame) => {
                        let json = serde_json::to_string(&frame).unwrap();
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

/// A source's original or picture (`original`, `page-{n}.png`, `frame-{ms}.jpg`), with
/// byte-range support so audio and video can seek. Like the service, the signature is
/// required; unlike it, any signature passes.
async fn source_media(
    State(fake): State<Fake>,
    Path((id, file)): Path<(String, String)>,
    Query(query): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Response {
    if !query.contains_key("exp") || !query.contains_key("sig") {
        return api_error(
            StatusCode::FORBIDDEN,
            "bad_signature",
            "This link isn't valid.",
        );
    }
    let (source, uploaded) = {
        let s = fake.state.lock().unwrap();
        (
            s.library.sources.iter().find(|src| src.id == id).cloned(),
            s.source_files.get(&id).cloned(),
        )
    };
    let Some(source) = source else {
        return not_found();
    };
    let bytes = if let Some(path) = sample_media_file(&id, &file) {
        match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(_) => return not_found(),
        }
    } else if file == "original" {
        match (id.as_str(), uploaded) {
            ("s2", _) => GIBBS_MD.as_bytes().to_vec(),
            ("s7", _) => PARTIAL_SUMS_PY.as_bytes().to_vec(),
            (_, Some(bytes)) => bytes,
            _ => return not_found(),
        }
    } else {
        return not_found();
    };
    let mime = if file.ends_with(".png") {
        "image/png"
    } else if file.ends_with(".jpg") {
        "image/jpeg"
    } else {
        media_type(&source.name)
    };
    ranged(&headers, mime, bytes)
}

/// The media type a source file is served with, from its name.
fn media_type(name: &str) -> &'static str {
    let ext = name
        .rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "pdf" => "application/pdf",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "heic" | "heif" => "image/heic",
        "tif" | "tiff" => "image/tiff",
        "bmp" => "image/bmp",
        "mp3" => "audio/mpeg",
        "m4a" | "aac" => "audio/mp4",
        "wav" => "audio/wav",
        "aif" | "aiff" => "audio/aiff",
        "flac" => "audio/flac",
        "caf" => "audio/x-caf",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        _ => "text/plain; charset=utf-8",
    }
}

/// `bytes` as the whole body, or the part a `Range: bytes=a-b` header asks for (206).
fn ranged(headers: &HeaderMap, mime: &'static str, bytes: Vec<u8>) -> Response {
    let len = bytes.len();
    let range = headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("bytes="))
        .and_then(|v| v.split_once('-'))
        .and_then(|(a, b)| {
            let (start, end) = match (a.trim(), b.trim()) {
                ("", suffix) => {
                    let n: usize = suffix.parse().ok()?;
                    (len.checked_sub(n.min(len))?, len.checked_sub(1)?)
                }
                (a, "") => (a.parse().ok()?, len.checked_sub(1)?),
                (a, b) => (
                    a.parse().ok()?,
                    b.parse::<usize>().ok()?.min(len.checked_sub(1)?),
                ),
            };
            (start <= end && end < len).then_some((start, end))
        });
    match (headers.contains_key(header::RANGE), range) {
        (_, Some((start, end))) => (
            StatusCode::PARTIAL_CONTENT,
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
                (header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}")),
            ],
            bytes[start..=end].to_vec(),
        )
            .into_response(),
        (true, None) => (
            StatusCode::RANGE_NOT_SATISFIABLE,
            [(header::CONTENT_RANGE, format!("bytes */{len}"))],
        )
            .into_response(),
        (false, None) => (
            [
                (header::CONTENT_TYPE, mime.to_string()),
                (header::ACCEPT_RANGES, "bytes".to_string()),
            ],
            bytes,
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Chats: a scripted stand-in for the agent. A message is answered with a reasoning
// summary, a `search_library` call over the fake library and a short Markdown answer;
// "source" adds a `search_sources` call with a Passages card, "delete" asks to delete a
// note (a confirmation card), "fail" ends the turn on a retryable error. With
// `simulate` the answer streams in over a few seconds; otherwise it is written before
// the request is answered, or, with `chat_hang`, stops part-way and stays running.
// ---------------------------------------------------------------------------

/// A sample conversation (`c1`) and an interrupted one (`c2`), for manual runs and
/// screenshots. Media URLs are relative until `rebase_chat_media`.
pub fn sample_chats() -> Vec<(Chat, Vec<ChatItem>)> {
    let chat = |id: &str, title: &str, state, at| Chat {
        id: id.into(),
        title: title.into(),
        state,
        workspace_ids: vec![],
        source_ids: vec![],
        pending: None,
        usage: Usage {
            input_tokens: 18_400,
            cached_input_tokens: 12_000,
            output_tokens: 1_250,
            reasoning_tokens: 640,
            model_calls: 4,
        },
        created_at: ts(at),
        updated_at: ts(at + 30),
    };
    let item = |chat: &str, seq: u64, turn: &str, kind| ChatItem {
        id: format!("{chat}-i{seq}"),
        chat_id: chat.into(),
        turn_id: turn.into(),
        seq,
        status: ItemStatus::Done,
        created_at: ts(40 + seq as i64),
        updated_at: ts(40 + seq as i64),
        kind,
    };
    let user = |text: &str| ChatItemKind::UserMessage {
        text: text.into(),
        source_ids: vec![],
        device_id: "d1".into(),
        device_name: "test-mac".into(),
    };
    let call = |id: &str, name: &str, args: &str, label: &str| ChatItemKind::ToolCall {
        call_id: id.into(),
        name: name.into(),
        arguments: args.into(),
        label: label.into(),
        device: None,
    };
    let result = |id: &str, summary: &str| ChatItemKind::ToolResult {
        call_id: id.into(),
        ok: true,
        summary: summary.into(),
        output_bytes: sample_tool_output(id).len() as u64,
    };
    let passages = sample_citations().into_iter().take(3).collect();
    let c1 = vec![
        item("c1", 1, "t1", user("How do Fourier series converge?")),
        item(
            "c1",
            2,
            "t1",
            ChatItemKind::Reasoning {
                summary: "**Checking the library first**\n\nThe Fourier series topic has a lesson on why sums of sines work; convergence is covered there and in a note.".into(),
            },
        ),
        item(
            "c1",
            3,
            "t1",
            call(
                "call_1",
                "search_library",
                r#"{"query":"fourier convergence"}"#,
                "Searching the library for “fourier convergence”",
            ),
        ),
        item("c1", 4, "t1", result("call_1", "2 matches: 1 lesson, 1 note")),
        item(
            "c1",
            5,
            "t1",
            ChatItemKind::AssistantText {
                text: "For a reasonably well-behaved periodic signal the partial sums\n\n$$S_N(t) = \\sum_{n=-N}^{N} c_n e^{i n \\omega_0 t}$$\n\nconverge to the signal wherever it is continuous, and to the **midpoint of the jump** where it isn't.\n\n- Your lesson *Why sums of sines?* walks through the square wave.\n- Your note *Whole-number multiples* has the key fact about frequencies.\n\nNear a jump the partial sums overshoot by about 9% however many terms you add (the Gibbs phenomenon).".into(),
            },
        ),
        item("c1", 6, "t2", user("Show me where my sources say that.")),
        item(
            "c1",
            7,
            "t2",
            call(
                "call_2",
                "search_sources",
                r#"{"query":"partial sums overshoot","topic_id":"fourier","source_ids":null}"#,
                "Searching the sources for “partial sums overshoot”",
            ),
        ),
        item("c1", 8, "t2", result("call_2", "3 passages in 3 files")),
        item(
            "c1",
            9,
            "t2",
            ChatItemKind::Card {
                call_id: "call_2".into(),
                card: Card::Passages(PassagesCard {
                    query: "partial sums overshoot".into(),
                    passages,
                }),
            },
        ),
        item(
            "c1",
            10,
            "t2",
            ChatItemKind::AssistantText {
                text: "Three of the topic's sources cover it: the textbook's chapter 3 [page 3], the script that plots the partial sums, and the square-wave figure.".into(),
            },
        ),
    ];
    let mut cancelled = item(
        "c2",
        2,
        "t3",
        ChatItemKind::AssistantText {
            text: "Eigenvectors are the directions a matrix only stretches".into(),
        },
    );
    cancelled.status = ItemStatus::Cancelled;
    let c2 = vec![
        item("c2", 1, "t3", user("Explain eigenvectors with a picture")),
        cancelled,
        item(
            "c2",
            3,
            "t3",
            ChatItemKind::Action {
                text: "The service stopped while answering.".into(),
            },
        ),
    ];
    vec![
        (
            chat("c1", "How do Fourier series converge?", ChatState::Idle, 40),
            c1,
        ),
        (
            chat(
                "c2",
                "Explain eigenvectors with a picture",
                ChatState::Interrupted {
                    turn_id: "t3".into(),
                },
                10,
            ),
            c2,
        ),
    ]
}

/// What the sample chat's tool calls returned (`call_1`, `call_2`).
pub fn sample_tool_output(call_id: &str) -> String {
    match call_id {
        "call_1" => serde_json::json!([
            {"kind": "lesson", "id": "l1", "title": "Why sums of sines?", "context": "Signals & Systems / Fourier series", "snippet": "Any periodic signal that's reasonably well behaved can be rebuilt from sine and cosine waves."},
            {"kind": "note", "id": "n1", "title": "Whole-number multiples", "context": "Signals & Systems / Fourier series", "snippet": "Every frequency is a whole-number multiple of the base frequency."}
        ])
        .to_string(),
        "call_2" => serde_json::to_string(&sample_citations().into_iter().take(3).collect::<Vec<_>>())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// Make the passages' relative media URLs absolute under `base`.
pub fn rebase_chat_media(items: &mut [ChatItem], base: impl Fn(&str) -> String) {
    for item in items {
        if let ChatItemKind::Card {
            card: Card::Passages(card),
            ..
        } = &mut item.kind
        {
            for c in &mut card.passages {
                if let Some(u) = c.image_url.as_mut()
                    && u.starts_with("/media/")
                {
                    *u = base(u);
                }
            }
        }
    }
}

impl FakeState {
    fn chat_mut(&mut self, id: &str) -> Option<&mut Chat> {
        self.chats.iter_mut().find(|c| c.id == id)
    }

    /// Adds an item to the end of a chat's transcript.
    fn add_item(
        &mut self,
        chat_id: &str,
        turn_id: &str,
        status: ItemStatus,
        kind: ChatItemKind,
    ) -> ChatItem {
        self.chat_seq += 1;
        let now = Utc::now();
        let item = ChatItem {
            id: self.id("item"),
            chat_id: chat_id.into(),
            turn_id: turn_id.into(),
            seq: self.chat_seq,
            status,
            created_at: now,
            updated_at: now,
            kind,
        };
        self.chat_items
            .entry(chat_id.into())
            .or_default()
            .push(item.clone());
        item
    }

    fn update_item(
        &mut self,
        chat_id: &str,
        item_id: &str,
        change: impl FnOnce(&mut ChatItem),
    ) -> Option<ChatItem> {
        let item = self
            .chat_items
            .get_mut(chat_id)?
            .iter_mut()
            .find(|i| i.id == item_id)?;
        change(item);
        item.updated_at = Utc::now();
        Some(item.clone())
    }

    /// Sets a chat's state; returns the changed chat.
    fn set_chat_state(&mut self, chat_id: &str, state: ChatState) -> Option<Chat> {
        let chat = self.chat_mut(chat_id)?;
        chat.state = state;
        chat.updated_at = Utc::now();
        Some(chat.clone())
    }

    /// Whether `turn_id` is still the chat's running turn (Stop or a delete ends it).
    fn turn_running(&self, chat_id: &str, turn_id: &str) -> bool {
        self.chats.iter().any(|c| {
            c.id == chat_id
                && matches!(&c.state, ChatState::Running { turn_id: t, .. } if t == turn_id)
        })
    }

    fn current_device(&self) -> (Id, String) {
        self.devices
            .iter()
            .find(|d| d.current)
            .map(|d| (d.id.clone(), d.name.clone()))
            .unwrap_or_else(|| ("d1".into(), "test-mac".into()))
    }

    /// Stores the user message and starts its turn: the chat is Running. Returns the
    /// events to publish and the item.
    fn start_turn(&mut self, chat_id: &str, message: NewMessage) -> (Vec<Event>, ChatItem) {
        let turn_id = self.id("turn");
        let (device_id, device_name) = self.current_device();
        let first = self.chat_items.get(chat_id).is_none_or(|i| i.is_empty());
        let item = self.add_item(
            chat_id,
            &turn_id,
            ItemStatus::Done,
            ChatItemKind::UserMessage {
                text: message.text.clone(),
                source_ids: message.source_ids.clone(),
                device_id: device_id.clone(),
                device_name,
            },
        );
        let chat = self.chat_mut(chat_id).expect("the chat exists");
        if first && chat.title.is_empty() {
            chat.title = chat_title(&message.text);
        }
        for id in message.source_ids {
            if !chat.source_ids.contains(&id) {
                chat.source_ids.push(id);
            }
        }
        chat.pending = None;
        chat.state = ChatState::Running {
            turn_id,
            device_id,
            started_at: Utc::now(),
        };
        chat.updated_at = Utc::now();
        let chat = chat.clone();
        (
            vec![
                Event::ChatItemChanged { item: item.clone() },
                Event::ChatChanged { chat },
            ],
            item,
        )
    }

    /// What Stop and a restart do to the turn's unfinished work: in-progress items are
    /// cancelled, calls without a result get one, a pending confirmation is denied.
    fn cancel_turn(&mut self, chat_id: &str, turn_id: &str, summary: &str) -> Vec<Event> {
        let (device_id, _) = self.current_device();
        let mut events = Vec::new();
        let items = self.chat_items.get(chat_id).cloned().unwrap_or_default();
        for item in items.iter().filter(|i| i.turn_id == turn_id) {
            if item.status == ItemStatus::InProgress
                && let Some(changed) = self.update_item(chat_id, &item.id, |i| {
                    i.status = ItemStatus::Cancelled;
                })
            {
                events.push(Event::ChatItemChanged { item: changed });
            }
            if let ChatItemKind::Card {
                card:
                    Card::Confirmation(ConfirmationCard {
                        decision: Decision::Pending,
                        ..
                    }),
                ..
            } = &item.kind
                && let Some(changed) = self.update_item(chat_id, &item.id, |i| {
                    if let ChatItemKind::Card {
                        card: Card::Confirmation(c),
                        ..
                    } = &mut i.kind
                    {
                        c.decision = Decision::Denied {
                            device_id: device_id.clone(),
                            by_stop: true,
                        };
                    }
                })
            {
                events.push(Event::ChatItemChanged { item: changed });
            }
        }
        for call_id in unanswered(&items, turn_id) {
            let result = self.add_item(
                chat_id,
                turn_id,
                ItemStatus::Done,
                ChatItemKind::ToolResult {
                    call_id,
                    ok: false,
                    summary: summary.into(),
                    output_bytes: 0,
                },
            );
            events.push(Event::ChatItemChanged { item: result });
        }
        events
    }
}

/// The calls of `turn_id` that have no result yet.
fn unanswered(items: &[ChatItem], turn_id: &str) -> Vec<String> {
    let answered: Vec<&str> = items
        .iter()
        .filter_map(|i| match &i.kind {
            ChatItemKind::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    items
        .iter()
        .filter(|i| i.turn_id == turn_id)
        .filter_map(|i| match &i.kind {
            ChatItemKind::ToolCall { call_id, .. } if !answered.contains(&call_id.as_str()) => {
                Some(call_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// A chat's title from its first message: the first line, cut to `CHAT_TITLE_MAX`.
fn chat_title(text: &str) -> String {
    let line = text.trim().lines().next().unwrap_or("").trim();
    if line.chars().count() <= CHAT_TITLE_MAX {
        line.to_string()
    } else {
        let cut: String = line.chars().take(CHAT_TITLE_MAX - 1).collect();
        format!("{}…", cut.trim_end())
    }
}

fn publish_all(fake: &Fake, events: Vec<Event>) {
    for event in events {
        publish(fake, event);
    }
}

/// Pause between steps of a streamed answer (manual runs only).
async fn pace(fake: &Fake, ms: u64) {
    let simulate = fake.state.lock().unwrap().simulate;
    if simulate {
        tokio::time::sleep(Duration::from_millis(ms)).await;
    }
}

/// Runs one step of a turn under the lock, if the turn is still running; publishes what
/// it returns. `None` when the turn was stopped or the chat deleted meanwhile.
fn step<T>(
    fake: &Fake,
    chat_id: &str,
    turn_id: &str,
    f: impl FnOnce(&mut FakeState) -> (Vec<Event>, T),
) -> Option<T> {
    let (events, out) = {
        let mut s = fake.state.lock().unwrap();
        if !s.turn_running(chat_id, turn_id) {
            return None;
        }
        f(&mut s)
    };
    publish_all(fake, events);
    Some(out)
}

/// Streams `text` into an in-progress item, a few words at a time.
async fn stream_into(
    fake: &Fake,
    chat_id: &str,
    turn_id: &str,
    item_id: &str,
    text: &str,
) -> Option<()> {
    let mut pieces = Vec::new();
    let mut piece = String::new();
    for (n, word) in text.split_inclusive(' ').enumerate() {
        piece.push_str(word);
        if n % 3 == 2 {
            pieces.push(std::mem::take(&mut piece));
        }
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    for piece in pieces {
        step(fake, chat_id, turn_id, |s| {
            let mut offset = 0;
            let item = s.update_item(chat_id, item_id, |i| {
                offset = i.kind.streaming_text().map_or(0, str::len) as u64;
                i.kind.apply_delta(offset, &piece);
            });
            let events = item
                .map(|_| {
                    vec![Event::ChatItemDelta {
                        chat_id: chat_id.into(),
                        item_id: item_id.into(),
                        offset,
                        text: piece.clone(),
                    }]
                })
                .unwrap_or_default();
            (events, ())
        })?;
        pace(fake, 60).await;
    }
    Some(())
}

/// Adds an in-progress item, streams `text` into it, and marks it done.
async fn streamed_item(
    fake: &Fake,
    chat_id: &str,
    turn_id: &str,
    empty: ChatItemKind,
    text: &str,
) -> Option<ChatItem> {
    let item = step(fake, chat_id, turn_id, |s| {
        let item = s.add_item(chat_id, turn_id, ItemStatus::InProgress, empty);
        (vec![Event::ChatItemChanged { item: item.clone() }], item)
    })?;
    stream_into(fake, chat_id, turn_id, &item.id, text).await?;
    step(fake, chat_id, turn_id, |s| {
        let done = s.update_item(chat_id, &item.id, |i| i.status = ItemStatus::Done);
        let events = done
            .clone()
            .map(|item| vec![Event::ChatItemChanged { item }])
            .unwrap_or_default();
        (events, done)
    })?
}

#[allow(clippy::too_many_arguments)]
/// A tool call whose arguments stream in, then its result (with the full output kept
/// for `chat_item_output`) and the call marked done.
async fn tool_call(
    fake: &Fake,
    chat_id: &str,
    turn_id: &str,
    name: &str,
    arguments: serde_json::Value,
    label: String,
    summary: String,
    output: String,
) -> Option<String> {
    let call_id = step(fake, chat_id, turn_id, |s| {
        let call_id = s.id("call");
        (vec![], call_id)
    })?;
    let call = streamed_item(
        fake,
        chat_id,
        turn_id,
        ChatItemKind::ToolCall {
            call_id: call_id.clone(),
            name: name.into(),
            arguments: String::new(),
            label: String::new(),
            device: None,
        },
        &arguments.to_string(),
    )
    .await?;
    // `streamed_item` marked it done; a call stays in progress until its result.
    step(fake, chat_id, turn_id, |s| {
        let item = s.update_item(chat_id, &call.id, |i| {
            i.status = ItemStatus::InProgress;
            if let ChatItemKind::ToolCall { label: l, .. } = &mut i.kind {
                *l = label.clone();
            }
        });
        (
            item.map(|item| vec![Event::ChatItemChanged { item }])
                .unwrap_or_default(),
            (),
        )
    })?;
    pace(fake, 500).await;
    step(fake, chat_id, turn_id, |s| {
        let result = s.add_item(
            chat_id,
            turn_id,
            ItemStatus::Done,
            ChatItemKind::ToolResult {
                call_id: call_id.clone(),
                ok: true,
                summary,
                output_bytes: output.len() as u64,
            },
        );
        s.chat_outputs.insert(result.id.clone(), output);
        let mut events = vec![Event::ChatItemChanged { item: result }];
        if let Some(item) = s.update_item(chat_id, &call.id, |i| i.status = ItemStatus::Done) {
            events.push(Event::ChatItemChanged { item });
        }
        (events, ())
    })?;
    Some(call_id)
}

/// The library's lessons and notes that mention any word of `prompt`.
fn library_matches(s: &FakeState, prompt: &str) -> Vec<SearchHit> {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() > 3)
        .map(str::to_lowercase)
        .collect();
    let mentions = |text: &str| {
        let text = text.to_lowercase();
        words.iter().any(|w| text.contains(w.as_str()))
    };
    let topic = |id: &str| {
        s.library
            .topics
            .iter()
            .find(|t| t.id == id)
            .map(|t| t.title.clone())
            .unwrap_or_default()
    };
    let mut hits = Vec::new();
    for l in &s.library.lessons {
        if mentions(&l.title) || mentions(&l.body_md) {
            hits.push(SearchHit {
                kind: SearchKind::Lesson,
                id: l.id.clone(),
                title: l.title.clone(),
                context: topic(&l.topic_id),
                snippet: l.summary.clone(),
            });
        }
    }
    for n in &s.library.notes {
        if mentions(&n.title) || mentions(&n.body_md) {
            hits.push(SearchHit {
                kind: SearchKind::Note,
                id: n.id.clone(),
                title: n.title.clone(),
                context: topic(&n.topic_id),
                snippet: n.body_md.chars().take(120).collect(),
            });
        }
    }
    hits
}

/// The scripted answer to `prompt` (see the section comment). `retry` is a Continue or
/// Retry, which never fails again.
async fn run_answer(fake: Fake, chat_id: Id, turn_id: Id, prompt: String, retry: bool) {
    let (chat_id, turn_id) = (chat_id.as_str(), turn_id.as_str());
    let lower = prompt.to_lowercase();
    let no_key = !fake.state.lock().unwrap().has_api_key;
    if no_key || (lower.contains("fail") && !retry) {
        let message = if no_key {
            "Add an OpenAI API key in Settings first."
        } else {
            "The model API answered 500 three times."
        };
        step(&fake, chat_id, turn_id, |s| {
            let error = s.add_item(
                chat_id,
                turn_id,
                ItemStatus::Done,
                ChatItemKind::Error {
                    message: message.into(),
                    retryable: true,
                },
            );
            let chat = s.set_chat_state(
                chat_id,
                ChatState::Failed {
                    turn_id: turn_id.into(),
                    error: message.into(),
                },
            );
            let mut events = vec![Event::ChatItemChanged { item: error }];
            events.extend(chat.map(|chat| Event::ChatChanged { chat }));
            (events, ())
        });
        start_pending(&fake, chat_id).await;
        return;
    }
    pace(&fake, 300).await;
    let Some(_) = streamed_item(
        &fake,
        chat_id,
        turn_id,
        ChatItemKind::Reasoning {
            summary: String::new(),
        },
        "**Checking the library first**\n\nLooking for lessons and notes that already cover this.",
    )
    .await
    else {
        return;
    };

    if lower.contains("delete") {
        let note = fake.state.lock().unwrap().library.notes.first().cloned();
        if let Some(note) = note {
            let args = serde_json::json!({ "note_id": note.id });
            let Some(call_id) = step(&fake, chat_id, turn_id, |s| (vec![], s.id("call"))) else {
                return;
            };
            step(&fake, chat_id, turn_id, |s| {
                let call = s.add_item(
                    chat_id,
                    turn_id,
                    ItemStatus::InProgress,
                    ChatItemKind::ToolCall {
                        call_id: call_id.clone(),
                        name: "delete_note".into(),
                        arguments: args.to_string(),
                        label: format!("Deleting the note “{}”", note.title),
                        device: None,
                    },
                );
                let card = s.add_item(
                    chat_id,
                    turn_id,
                    ItemStatus::Done,
                    ChatItemKind::Card {
                        call_id: call_id.clone(),
                        card: Card::Confirmation(ConfirmationCard {
                            tool: "delete_note".into(),
                            title: format!("Delete the note “{}”?", note.title),
                            detail: "The note is deleted. This can't be undone.".into(),
                            approve_label: "Delete".into(),
                            decision: Decision::Pending,
                        }),
                    },
                );
                let (device_id, _) = s.current_device();
                let chat = s.set_chat_state(
                    chat_id,
                    ChatState::WaitingForUser {
                        turn_id: turn_id.into(),
                        device_id,
                        item_id: card.id.clone(),
                    },
                );
                let mut events = vec![
                    Event::ChatItemChanged { item: call },
                    Event::ChatItemChanged { item: card },
                ];
                events.extend(chat.map(|chat| Event::ChatChanged { chat }));
                (events, ())
            });
            return;
        }
    }

    let (hits, query) = {
        let s = fake.state.lock().unwrap();
        (library_matches(&s, &prompt), prompt.trim().to_string())
    };
    let summary = match hits.len() {
        0 => "No matches".to_string(),
        1 => "1 match".to_string(),
        n => format!("{n} matches"),
    };
    if tool_call(
        &fake,
        chat_id,
        turn_id,
        "search_library",
        serde_json::json!({ "query": query }),
        format!("Searching the library for “{query}”"),
        summary,
        serde_json::to_string(&hits).unwrap_or_default(),
    )
    .await
    .is_none()
    {
        return;
    }

    if lower.contains("source") {
        let passages: Vec<Citation> = {
            let s = fake.state.lock().unwrap();
            s.library
                .lessons
                .iter()
                .flat_map(|l| l.citations.iter().cloned())
                .take(3)
                .collect()
        };
        let output = serde_json::to_string(&passages).unwrap_or_default();
        let Some(call_id) = tool_call(
            &fake,
            chat_id,
            turn_id,
            "search_sources",
            serde_json::json!({ "query": query, "topic_id": null, "source_ids": null }),
            format!("Searching the sources for “{query}”"),
            format!("{} passages", passages.len()),
            output,
        )
        .await
        else {
            return;
        };
        if step(&fake, chat_id, turn_id, |s| {
            let card = s.add_item(
                chat_id,
                turn_id,
                ItemStatus::Done,
                ChatItemKind::Card {
                    call_id,
                    card: Card::Passages(PassagesCard {
                        query: query.clone(),
                        passages,
                    }),
                },
            );
            (vec![Event::ChatItemChanged { item: card }], ())
        })
        .is_none()
        {
            return;
        }
    }

    let answer = if hits.is_empty() {
        format!(
            "Nothing in your library mentions **{query}** yet. Ask me to outline a course on it, or add sources to a topic first."
        )
    } else {
        let mut text = format!("Here's what your library has on **{query}**:\n\n");
        for hit in &hits {
            let kind = if hit.kind == SearchKind::Lesson {
                "lesson"
            } else {
                "note"
            };
            text.push_str(&format!(
                "- The {kind} *{}* in {}\n",
                hit.title, hit.context
            ));
        }
        text.push_str("\nAsk me to go deeper into any of them.");
        text
    };
    pace(&fake, 300).await;
    let hang = fake.state.lock().unwrap().chat_hang;
    if hang {
        // Part of the answer, then nothing until Stop.
        step(&fake, chat_id, turn_id, |s| {
            let item = s.add_item(
                chat_id,
                turn_id,
                ItemStatus::InProgress,
                ChatItemKind::AssistantText {
                    text: "Here's what".into(),
                },
            );
            (vec![Event::ChatItemChanged { item }], ())
        });
        return;
    }
    if streamed_item(
        &fake,
        chat_id,
        turn_id,
        ChatItemKind::AssistantText {
            text: String::new(),
        },
        &answer,
    )
    .await
    .is_none()
    {
        return;
    }
    finish_turn(&fake, chat_id, turn_id).await;
}

/// The turn ended: usage added, the chat idle, then the pending message, if any.
async fn finish_turn(fake: &Fake, chat_id: &str, turn_id: &str) {
    let done = step(fake, chat_id, turn_id, |s| {
        let chat = s.chat_mut(chat_id).map(|chat| {
            chat.usage.add(&Usage {
                input_tokens: 4_200,
                cached_input_tokens: 3_100,
                output_tokens: 310,
                reasoning_tokens: 120,
                model_calls: 2,
            });
            chat.state = ChatState::Idle;
            chat.updated_at = Utc::now();
            chat.clone()
        });
        (
            chat.map(|chat| vec![Event::ChatChanged { chat }])
                .unwrap_or_default(),
            (),
        )
    });
    if done.is_some() {
        start_pending(fake, chat_id).await;
    }
}

/// Sends the chat's pending message as the next turn, if it has one and isn't busy.
async fn start_pending(fake: &Fake, chat_id: &str) {
    let started = {
        let mut s = fake.state.lock().unwrap();
        let pending = s
            .chats
            .iter()
            .find(|c| c.id == chat_id && !c.state.is_busy())
            .and_then(|c| c.pending.clone());
        pending.map(|p| {
            let (events, item) = s.start_turn(
                chat_id,
                NewMessage {
                    text: p.text.clone(),
                    source_ids: p.source_ids,
                },
            );
            (events, item.turn_id, p.text)
        })
    };
    if let Some((events, turn_id, text)) = started {
        publish_all(fake, events);
        run_turn_task(fake.clone(), chat_id.into(), turn_id, text, false).await;
    }
}

/// Runs the answer: in the background while simulating, else before returning. Boxed:
/// an answer ends by starting the pending message's turn.
fn run_turn_task(
    fake: Fake,
    chat_id: Id,
    turn_id: Id,
    text: String,
    retry: bool,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let simulate = fake.state.lock().unwrap().simulate;
        let task = run_answer(fake, chat_id, turn_id, text, retry);
        if simulate {
            tokio::spawn(task);
        } else {
            task.await;
        }
    })
}

#[allow(clippy::result_large_err)]
fn check_message(message: &NewMessage) -> Result<(), Response> {
    if message.text.trim().is_empty() {
        return Err(api_error(
            StatusCode::BAD_REQUEST,
            "empty_message",
            "Write a message first.",
        ));
    }
    Ok(())
}

fn chat_detail(s: &FakeState, id: &str) -> Option<ChatDetail> {
    let chat = s.chats.iter().find(|c| c.id == id)?.clone();
    let items = s.chat_items.get(id).cloned().unwrap_or_default();
    Some(ChatDetail { chat, items })
}

async fn list_chats(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let mut chats = fake.state.lock().unwrap().chats.clone();
    chats.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    Json(chats).into_response()
}

async fn create_chat(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<NewChat>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    if let Some(message) = &req.message
        && let Err(r) = check_message(message)
    {
        return r;
    }
    let (events, detail, turn) = {
        let mut s = fake.state.lock().unwrap();
        let now = Utc::now();
        let chat = Chat {
            id: s.id("chat"),
            title: String::new(),
            state: ChatState::Idle,
            workspace_ids: req.workspace_ids,
            source_ids: vec![],
            pending: None,
            usage: Usage::default(),
            created_at: now,
            updated_at: now,
        };
        let id = chat.id.clone();
        s.chats.push(chat.clone());
        s.chat_items.insert(id.clone(), vec![]);
        match req.message {
            Some(message) => {
                let text = message.text.clone();
                let (events, item) = s.start_turn(&id, message);
                let detail = chat_detail(&s, &id).unwrap();
                (events, detail, Some((id, item.turn_id, text)))
            }
            None => (
                vec![Event::ChatChanged { chat: chat.clone() }],
                ChatDetail {
                    chat,
                    items: vec![],
                },
                None,
            ),
        }
    };
    publish_all(&fake, events);
    if let Some((id, turn_id, text)) = turn {
        run_turn_task(fake.clone(), id, turn_id, text, false).await;
    }
    Json(detail).into_response()
}

async fn get_chat(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    match chat_detail(&fake.state.lock().unwrap(), &id) {
        Some(detail) => Json(detail).into_response(),
        None => not_found(),
    }
}

async fn patch_chat(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ChatPatch>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let chat = {
        let mut s = fake.state.lock().unwrap();
        let Some(chat) = s.chat_mut(&id) else {
            return not_found();
        };
        if let Some(title) = req.title {
            let title = title.trim();
            if title.is_empty() {
                return api_error(
                    StatusCode::BAD_REQUEST,
                    "empty_title",
                    "A chat needs a title.",
                );
            }
            chat.title = title.chars().take(200).collect();
        }
        if let Some(ids) = req.workspace_ids {
            chat.workspace_ids = ids;
        }
        if let Some(ids) = req.source_ids {
            chat.source_ids = ids;
        }
        chat.updated_at = Utc::now();
        chat.clone()
    };
    publish(&fake, Event::ChatChanged { chat: chat.clone() });
    Json(chat).into_response()
}

async fn delete_chat(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    {
        let mut s = fake.state.lock().unwrap();
        let before = s.chats.len();
        s.chats.retain(|c| c.id != id);
        if s.chats.len() == before {
            return not_found();
        }
        if let Some(items) = s.chat_items.remove(&id) {
            for item in items {
                s.chat_outputs.remove(&item.id);
            }
        }
    }
    publish(&fake, Event::ChatDeleted { id });
    StatusCode::NO_CONTENT.into_response()
}

async fn send_message(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<NewMessage>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    if let Err(r) = check_message(&req) {
        return r;
    }
    let (events, sent, turn) = {
        let mut s = fake.state.lock().unwrap();
        let (device_id, _) = s.current_device();
        let Some(chat) = s.chat_mut(&id) else {
            return not_found();
        };
        if chat.pending.is_some() {
            return api_error(
                StatusCode::CONFLICT,
                "already_pending",
                "A message is already waiting to be sent.",
            );
        }
        if chat.state.is_busy() {
            chat.pending = Some(PendingMessage {
                text: req.text,
                source_ids: req.source_ids,
                device_id,
                created_at: Utc::now(),
            });
            chat.updated_at = Utc::now();
            let chat = chat.clone();
            (
                vec![Event::ChatChanged { chat: chat.clone() }],
                MessageSent { chat, item: None },
                None,
            )
        } else {
            let text = req.text.clone();
            let (events, item) = s.start_turn(&id, req);
            let chat = s.chats.iter().find(|c| c.id == id).unwrap().clone();
            let turn = Some((item.turn_id.clone(), text));
            (
                events,
                MessageSent {
                    chat,
                    item: Some(item),
                },
                turn,
            )
        }
    };
    publish_all(&fake, events);
    if let Some((turn_id, text)) = turn {
        run_turn_task(fake.clone(), id, turn_id, text, false).await;
    }
    Json(sent).into_response()
}

async fn drop_pending(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let chat = {
        let mut s = fake.state.lock().unwrap();
        let Some(chat) = s.chat_mut(&id) else {
            return not_found();
        };
        chat.pending = None;
        chat.updated_at = Utc::now();
        chat.clone()
    };
    publish(&fake, Event::ChatChanged { chat: chat.clone() });
    Json(chat).into_response()
}

async fn stop_chat(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (events, chat) = {
        let mut s = fake.state.lock().unwrap();
        let Some(chat) = s.chats.iter().find(|c| c.id == id).cloned() else {
            return not_found();
        };
        let (true, Some(turn_id)) = (chat.state.is_busy(), chat.state.turn_id()) else {
            return api_error(
                StatusCode::CONFLICT,
                "not_running",
                "Nothing is being answered.",
            );
        };
        let turn_id = turn_id.to_string();
        let mut events = s.cancel_turn(&id, &turn_id, "Cancelled");
        let action = s.add_item(
            &id,
            &turn_id,
            ItemStatus::Done,
            ChatItemKind::Action {
                text: "Stopped by the owner.".into(),
            },
        );
        events.push(Event::ChatItemChanged { item: action });
        let chat = s.set_chat_state(&id, ChatState::Idle).unwrap();
        events.push(Event::ChatChanged { chat: chat.clone() });
        (events, chat)
    };
    publish_all(&fake, events);
    start_pending(&fake, &id).await;
    let chat = fake
        .state
        .lock()
        .unwrap()
        .chats
        .iter()
        .find(|c| c.id == id)
        .cloned()
        .unwrap_or(chat);
    Json(chat).into_response()
}

async fn continue_chat(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (chat, turn_id, text) = {
        let mut s = fake.state.lock().unwrap();
        let Some(chat) = s.chats.iter().find(|c| c.id == id).cloned() else {
            return not_found();
        };
        let (true, Some(turn_id)) = (chat.state.can_continue(), chat.state.turn_id()) else {
            return api_error(
                StatusCode::CONFLICT,
                "cannot_continue",
                "There's nothing to continue.",
            );
        };
        let turn_id = turn_id.to_string();
        let text = s
            .chat_items
            .get(&id)
            .and_then(|items| {
                items.iter().find_map(|i| match &i.kind {
                    ChatItemKind::UserMessage { text, .. } if i.turn_id == turn_id => {
                        Some(text.clone())
                    }
                    _ => None,
                })
            })
            .unwrap_or_default();
        let (device_id, _) = s.current_device();
        let chat = s
            .set_chat_state(
                &id,
                ChatState::Running {
                    turn_id: turn_id.clone(),
                    device_id,
                    started_at: Utc::now(),
                },
            )
            .unwrap();
        (chat, turn_id, text)
    };
    publish(&fake, Event::ChatChanged { chat: chat.clone() });
    run_turn_task(fake.clone(), id, turn_id, text, true).await;
    Json(chat).into_response()
}

async fn confirm(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<ConfirmRequest>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let (events, chat, turn_id) = {
        let mut s = fake.state.lock().unwrap();
        let Some(chat) = s.chats.iter().find(|c| c.id == id).cloned() else {
            return not_found();
        };
        let ChatState::WaitingForUser {
            turn_id, item_id, ..
        } = &chat.state
        else {
            return api_error(
                StatusCode::CONFLICT,
                "not_waiting",
                "That question was already answered.",
            );
        };
        if *item_id != req.item_id {
            return api_error(
                StatusCode::CONFLICT,
                "not_waiting",
                "That question was already answered.",
            );
        }
        let turn_id = turn_id.clone();
        let (device_id, _) = s.current_device();
        let mut call_id = String::new();
        let card = s
            .update_item(&id, &req.item_id, |i| {
                if let ChatItemKind::Card {
                    call_id: c,
                    card: Card::Confirmation(card),
                } = &mut i.kind
                {
                    call_id = c.clone();
                    card.decision = if req.approve {
                        Decision::Approved {
                            device_id: device_id.clone(),
                        }
                    } else {
                        Decision::Denied {
                            device_id: device_id.clone(),
                            by_stop: false,
                        }
                    };
                }
            })
            .unwrap();
        let mut events = vec![Event::ChatItemChanged { item: card }];
        // The note the call names goes, if approved.
        let note_id = s.chat_items.get(&id).and_then(|items| {
            items.iter().find_map(|i| match &i.kind {
                ChatItemKind::ToolCall {
                    call_id: c,
                    arguments,
                    ..
                } if *c == call_id => serde_json::from_str::<serde_json::Value>(arguments)
                    .ok()
                    .and_then(|v| v["note_id"].as_str().map(str::to_string)),
                _ => None,
            })
        });
        if req.approve
            && let Some(note_id) = &note_id
        {
            s.library.notes.retain(|n| &n.id != note_id);
            events.push(Event::NoteDeleted {
                id: note_id.clone(),
            });
        }
        let (summary, output) = if req.approve {
            ("Deleted", r#"{"deleted":true}"#.to_string())
        } else {
            (
                "Declined",
                r#"{"error":"The owner declined: delete the note"}"#.to_string(),
            )
        };
        let result = s.add_item(
            &id,
            &turn_id,
            ItemStatus::Done,
            ChatItemKind::ToolResult {
                call_id: call_id.clone(),
                ok: req.approve,
                summary: summary.into(),
                output_bytes: output.len() as u64,
            },
        );
        s.chat_outputs.insert(result.id.clone(), output);
        events.push(Event::ChatItemChanged { item: result });
        let call = s.chat_items.get(&id).and_then(|items| {
            items
                .iter()
                .find(|i| matches!(&i.kind, ChatItemKind::ToolCall { call_id: c, .. } if *c == call_id))
                .map(|i| i.id.clone())
        });
        if let Some(call) = call
            && let Some(item) = s.update_item(&id, &call, |i| i.status = ItemStatus::Done)
        {
            events.push(Event::ChatItemChanged { item });
        }
        let chat = s
            .set_chat_state(
                &id,
                ChatState::Running {
                    turn_id: turn_id.clone(),
                    device_id,
                    started_at: Utc::now(),
                },
            )
            .unwrap();
        events.push(Event::ChatChanged { chat: chat.clone() });
        (events, chat, turn_id)
    };
    publish_all(&fake, events);
    let text = if req.approve {
        "Done: the note is deleted."
    } else {
        "All right, I left the note as it is."
    };
    let task = {
        let fake = fake.clone();
        let id = id.clone();
        async move {
            if streamed_item(
                &fake,
                &id,
                &turn_id,
                ChatItemKind::AssistantText {
                    text: String::new(),
                },
                text,
            )
            .await
            .is_some()
            {
                finish_turn(&fake, &id, &turn_id).await;
            }
        }
    };
    let simulate = fake.state.lock().unwrap().simulate;
    if simulate {
        tokio::spawn(task);
    } else {
        task.await;
    }
    Json(chat).into_response()
}

async fn chat_item_output(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path((id, item)): Path<(String, String)>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let s = fake.state.lock().unwrap();
    let is_result = s.chat_items.get(&id).is_some_and(|items| {
        items
            .iter()
            .any(|i| i.id == item && matches!(i.kind, ChatItemKind::ToolResult { .. }))
    });
    if !is_result {
        return not_found();
    }
    let text = s.chat_outputs.get(&item).cloned().unwrap_or_default();
    Json(ToolOutputText { text }).into_response()
}

async fn list_workspaces(State(fake): State<Fake>, headers: HeaderMap) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    Json(fake.state.lock().unwrap().workspaces.clone()).into_response()
}

async fn create_workspace(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(req): Json<NewWorkspace>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    let workspace = {
        let mut s = fake.state.lock().unwrap();
        let (device_id, device_name) = s.current_device();
        let workspace = Workspace {
            id: s.id("workspace"),
            name: req.name,
            device_id,
            device_name,
            platform: req.platform,
            location: req.location,
            created_at: Utc::now(),
        };
        s.workspaces.push(workspace.clone());
        workspace
    };
    publish(
        &fake,
        Event::WorkspaceChanged {
            workspace: workspace.clone(),
        },
    );
    Json(workspace).into_response()
}

async fn delete_workspace(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Response {
    if let Err(r) = authorized(&headers) {
        return r;
    }
    {
        let mut s = fake.state.lock().unwrap();
        let before = s.workspaces.len();
        s.workspaces.retain(|w| w.id != id);
        if s.workspaces.len() == before {
            return not_found();
        }
    }
    publish(&fake, Event::WorkspaceDeleted { id });
    StatusCode::NO_CONTENT.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serves_source_media_with_ranges() {
        let service = FakeService::start().await;
        let lib = service.state.lock().unwrap().library.clone();
        let audio = lib.sources.iter().find(|s| s.id == "s5").unwrap();
        let url = audio.media_url.clone().unwrap();
        assert!(url.starts_with(&service.url), "{url}");
        let page = lib.lessons[0].citations[0].image_url.clone().unwrap();
        assert_eq!(
            page,
            format!("{}/media/sources/s1/page-3.png?exp=1&sig=x", service.url)
        );

        let client = reqwest::Client::new();
        let whole = client.get(&url).send().await.unwrap();
        assert_eq!(whole.status(), 200);
        assert_eq!(whole.headers()["content-type"], "audio/mp4");
        let len = whole.bytes().await.unwrap().len();
        let part = client
            .get(&url)
            .header("Range", "bytes=10-19")
            .send()
            .await
            .unwrap();
        assert_eq!(part.status(), 206);
        assert_eq!(
            part.headers()["content-range"],
            format!("bytes 10-19/{len}").as_str()
        );
        assert_eq!(part.bytes().await.unwrap().len(), 10);
        let png = client.get(&page).send().await.unwrap();
        assert_eq!(png.headers()["content-type"], "image/png");
        let unsigned = client
            .get(format!("{}/media/sources/s1/original", service.url))
            .send()
            .await
            .unwrap();
        assert_eq!(unsigned.status(), 403);
        let text = client
            .get(format!(
                "{}/media/sources/s2/original?exp=1&sig=x",
                service.url
            ))
            .send()
            .await
            .unwrap();
        assert!(text.text().await.unwrap().contains("Gibbs overshoot"));
    }

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
