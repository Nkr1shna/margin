//! Assembles the service: state, routes, background workers, recovery, serving.

use std::collections::HashSet;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{DefaultBodyLimit, Request, State};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, patch, post};
use axum::{Router, http::StatusCode};
use chrono::Utc;
use margin_embed::ModelFiles;
use margin_shared::{
    Chat, LessonStatus, Proposal, ProposalStatus, Source, SourceStatus, VideoStatus, routes,
};
use tokio::net::TcpListener;
use tokio::sync::{Semaphore, broadcast, mpsc, watch};

use crate::auth::{self, RateLimiter};
use crate::config::Config;
use crate::db::{self, Store};
use crate::devices::DeviceHub;
use crate::embedding::ModelJob;
use crate::engines::{Generator, Renderer, SourceTools};
use crate::error::AppError;
use crate::secrets::{SecretStore, media_signing_key};
use crate::state::{App, AppState, INTERRUPTED, LESSON_CONCURRENCY, StoredLesson, StoredVideo};
use crate::turns::Turns;
use crate::{
    ask, chats, embedding, events, library, media, search, settings, sources, turns, uploads,
    video, workspaces,
};

pub struct Options {
    pub config: Config,
    pub store: Store,
    pub secrets: Arc<dyn SecretStore>,
    pub generator: Arc<dyn Generator>,
    /// The embedding model and source reading (`EmbedTools` in production).
    pub source_tools: Arc<dyn SourceTools>,
    /// Where the embedding model is downloaded from (`margin_embed::HF_BASE`).
    pub hf_base: String,
    pub renderer: Arc<dyn Renderer>,
    pub allow_unsandboxed_render: bool,
}

/// A started service: its state and router, plus the handle that stops it.
pub struct Service {
    pub state: AppState,
    pub router: Router,
    shutdown: watch::Sender<bool>,
}

impl Service {
    /// Builds the state, runs startup recovery and starts the background workers.
    pub async fn start(opts: Options) -> anyhow::Result<Self> {
        let media_key = {
            let secrets = opts.secrets.clone();
            tokio::task::spawn_blocking(move || media_signing_key(secrets.as_ref())).await??
        };
        std::fs::create_dir_all(opts.config.videos_dir())?;
        std::fs::create_dir_all(opts.config.sources_dir())?;
        let (events, _) = broadcast::channel(1024);
        let (revocations, _) = broadcast::channel(64);
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let (video_tx, video_rx) = mpsc::unbounded_channel();
        let (source_tx, source_rx) = mpsc::unbounded_channel();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(concat!("margin-service/", env!("CARGO_PKG_VERSION")))
            .build()?;
        // The model is hundreds of MB: no overall limit, only for a stalled connection.
        let download_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .user_agent(concat!("margin-service/", env!("CARGO_PKG_VERSION")))
            .build()?;
        let model_files = ModelFiles::new(opts.config.embedding_model_dir());
        let state: AppState = Arc::new(App {
            config: opts.config,
            store: opts.store,
            secrets: opts.secrets,
            generator: opts.generator,
            renderer: opts.renderer,
            allow_unsandboxed_render: opts.allow_unsandboxed_render,
            http,
            media_key,
            events,
            revocations,
            shutdown: shutdown_rx,
            write_lock: tokio::sync::Mutex::new(()),
            logins: Mutex::new(Default::default()),
            limiter: RateLimiter::default(),
            lesson_slots: Arc::new(Semaphore::new(LESSON_CONCURRENCY)),
            lesson_jobs: Mutex::new(Default::default()),
            video_queue: video_tx,
            render_job: Mutex::new(None),
            source_tools: opts.source_tools,
            hf_base: opts.hf_base,
            download_http,
            model_files,
            embedder: tokio::sync::Mutex::new(None),
            model_job: Mutex::new(ModelJob::Idle),
            model_download_bytes: Mutex::new(None),
            source_queue: source_tx,
            uploads_busy: Mutex::new(Default::default()),
            turns: Turns::new(),
            devices: Arc::new(DeviceHub::new()),
        });

        let resume = recover(&state).await?;
        if let Err(e) = uploads::purge_stale(&state).await {
            tracing::warn!("couldn't clear unfinished uploads: {e:#}");
        }
        tokio::spawn(video::run_worker(state.clone(), video_rx));
        tokio::spawn(sources::run_worker(state.clone(), source_rx));
        tokio::spawn(housekeeping(state.clone()));
        for lesson_id in resume.lessons {
            ask::spawn_lesson_job(&state, lesson_id, true);
        }
        for video_id in resume.videos {
            let _ = state.video_queue.send(video_id);
        }
        for source_id in resume.sources {
            let _ = state.source_queue.send(source_id);
        }

        let router = router(state.clone());
        Ok(Self {
            state,
            router,
            shutdown: shutdown_tx,
        })
    }

    /// Serves until `signal` resolves, then closes event sockets and drains requests.
    pub async fn serve(
        self,
        listener: TcpListener,
        signal: impl Future<Output = ()> + Send + 'static,
    ) -> anyhow::Result<()> {
        let Service {
            router, shutdown, ..
        } = self;
        let stop = async move {
            signal.await;
            tracing::info!("shutting down");
            let _ = shutdown.send(true);
        };
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(stop)
        .await?;
        Ok(())
    }
}

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route(routes::ME, get(auth::me))
        .route(routes::LIBRARY, get(library::get_library))
        .route(routes::TOPICS, post(library::create_topic))
        .route(
            "/api/topics/{id}",
            patch(library::patch_topic).delete(library::delete_topic),
        )
        .route(
            "/api/lessons/{id}",
            patch(library::patch_lesson).delete(library::delete_lesson),
        )
        .route("/api/lessons/{id}/regenerate", post(ask::regenerate))
        .route("/api/lessons/{id}/video", post(video::request_video))
        .route(routes::NOTES, post(library::create_note))
        .route(
            "/api/notes/{id}",
            patch(library::patch_note).delete(library::delete_note),
        )
        .route("/api/proposals/{id}", get(ask::get_proposal))
        .route("/api/proposals/{id}/accept", post(ask::accept))
        .route("/api/videos/{id}", get(video::get_video))
        .route(routes::SEARCH, get(search::search))
        .route(
            routes::SETTINGS,
            get(settings::get_settings).patch(settings::patch_settings),
        )
        .route(routes::TOOLS, get(settings::tools))
        .route(routes::DEVICES, get(auth::list_devices))
        .route("/api/devices/{id}", delete(auth::revoke_device))
        // Upload bodies are streamed to disk, each against its kind's limit.
        .route(
            routes::SOURCES,
            post(sources::upload).layer(DefaultBodyLimit::disable()),
        )
        .route("/api/sources/{id}", delete(sources::delete_source))
        .route("/api/sources/{id}/reprocess", post(sources::reprocess))
        .route("/api/sources/{id}/transcript", get(sources::transcript))
        .route(routes::SOURCE_UPLOADS, post(uploads::start))
        .route(
            "/api/source-uploads/{id}",
            get(uploads::get)
                .put(uploads::put_part)
                .delete(uploads::cancel)
                .layer(DefaultBodyLimit::disable()),
        )
        .route("/api/source-uploads/{id}/finish", post(uploads::finish))
        .route(
            routes::EMBEDDING,
            get(embedding::get_model).delete(embedding::delete_model),
        )
        .route(routes::EMBEDDING_DOWNLOAD, post(embedding::start_download))
        .route(
            routes::CHATS,
            get(chats::list_chats).post(chats::create_chat),
        )
        .route(
            "/api/chats/{id}",
            get(chats::get_chat)
                .patch(chats::patch_chat)
                .delete(chats::delete_chat),
        )
        .route("/api/chats/{id}/messages", post(chats::post_message))
        .route("/api/chats/{id}/pending", delete(chats::cancel_pending))
        .route("/api/chats/{id}/stop", post(chats::stop))
        .route("/api/chats/{id}/continue", post(chats::continue_turn))
        .route("/api/chats/{id}/confirm", post(chats::confirm))
        .route(
            "/api/chats/{id}/items/{item_id}/output",
            get(chats::item_output),
        )
        .route(
            routes::WORKSPACES,
            get(workspaces::list_workspaces).post(workspaces::create_workspace),
        )
        .route("/api/workspaces/{id}", delete(workspaces::delete_workspace))
        .route(routes::EVENTS, get(events::events))
        .fallback(api_not_found)
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::require_device,
        ));

    Router::new()
        .route(routes::HEALTH, get(|| async { "ok" }))
        .route(routes::AUTH_START, post(auth::start))
        .route(routes::AUTH_POLL, post(auth::poll))
        .route(routes::AUTH_GITHUB_CALLBACK, get(auth::github_callback))
        .route("/media/videos/{id}/{file}", get(media::serve))
        .route("/media/sources/{id}/{file}", get(media::serve_source))
        .merge(api)
        .fallback(api_not_found)
        .layer(middleware::from_fn_with_state(state.clone(), log_requests))
        .with_state(state)
}

async fn api_not_found() -> AppError {
    AppError::new(StatusCode::NOT_FOUND, "not_found", "There's nothing here.")
}

/// Logs method, path (never the query string: it carries codes and signatures) and status.
async fn log_requests(State(_state): State<AppState>, req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let started = std::time::Instant::now();
    let res = next.run(req).await.into_response();
    tracing::debug!(%method, %path, status = res.status().as_u16(), ms = started.elapsed().as_millis() as u64, "request");
    res
}

/// Work to restart after recovery.
pub struct Resume {
    pub lessons: Vec<String>,
    pub videos: Vec<String>,
    pub sources: Vec<String>,
}

/// Jobs a crash left mid-way are marked failed ("Interrupted when the service
/// stopped."); lessons and videos still Queued are queued again. Sources being read are
/// read again from the start; uploads never attached to a topic or a chat are deleted
/// after 24 h. Chat turns left running become Interrupted (`turns::recover`).
pub async fn recover(state: &AppState) -> anyhow::Result<Resume> {
    let _w = state.write_lock.lock().await;
    let now = Utc::now();
    let mut resume = Resume {
        lessons: Vec::new(),
        videos: Vec::new(),
        sources: Vec::new(),
    };

    // Files attached to a chat stay while the chat exists.
    let chats: Vec<Chat> = state.store.list(db::CHAT).await?;
    let in_chats: HashSet<&str> = chats
        .iter()
        .flat_map(|c| c.source_ids.iter().map(String::as_str))
        .collect();
    let mut sources: Vec<Source> = state.store.list(db::SOURCE).await?;
    sources.sort_by_key(|s| s.created_at);
    for mut s in sources {
        if s.topic_id.is_none()
            && !in_chats.contains(s.id.as_str())
            && now - s.created_at > sources::UNATTACHED_TTL
        {
            tracing::info!(
                "deleting source {} ({}): never attached to a topic",
                s.id,
                s.name
            );
            sources::delete_locked(state, &s.id).await?;
            continue;
        }
        match s.status {
            SourceStatus::Processing { .. } => {
                s.status = SourceStatus::Queued;
                state.store.put(db::SOURCE, &s.id.clone(), &s).await?;
                resume.sources.push(s.id);
            }
            SourceStatus::Queued => resume.sources.push(s.id),
            _ => {}
        }
    }

    let mut lessons: Vec<StoredLesson> = state.store.list(db::LESSON).await?;
    lessons.sort_by(|a, b| {
        a.lesson
            .topic_id
            .cmp(&b.lesson.topic_id)
            .then(a.lesson.position.cmp(&b.lesson.position))
    });
    for mut row in lessons {
        match row.lesson.status {
            LessonStatus::Generating => {
                if let Some(prev) = row.previous_body.take() {
                    row.lesson.body_md = prev;
                }
                if let Some(prev) = row.previous_citations.take() {
                    row.lesson.citations = prev;
                }
                row.lesson.status = LessonStatus::Failed {
                    error: INTERRUPTED.into(),
                };
                row.lesson.updated_at = now;
                state
                    .store
                    .put(db::LESSON, &row.lesson.id.clone(), &row)
                    .await?;
            }
            LessonStatus::Queued => resume.lessons.push(row.lesson.id.clone()),
            _ => {}
        }
    }

    let mut videos: Vec<StoredVideo> = state.store.list(db::VIDEO).await?;
    videos.sort_by_key(|v| v.video.updated_at);
    for mut v in videos {
        match v.video.status {
            VideoStatus::Scripting
            | VideoStatus::Voicing { .. }
            | VideoStatus::Rendering { .. } => {
                v.video.status = VideoStatus::Failed {
                    error: INTERRUPTED.into(),
                };
                v.video.updated_at = now;
                state.store.put(db::VIDEO, &v.video.id.clone(), &v).await?;
            }
            VideoStatus::Queued => resume.videos.push(v.video.id.clone()),
            _ => {}
        }
    }

    let proposals: Vec<Proposal> = state.store.list(db::PROPOSAL).await?;
    for mut p in proposals {
        if p.status == ProposalStatus::Drafting {
            p.status = ProposalStatus::Failed {
                error: INTERRUPTED.into(),
            };
            state.store.put(db::PROPOSAL, &p.id.clone(), &p).await?;
        }
    }
    turns::recover(state).await?;
    if !resume.lessons.is_empty() || !resume.videos.is_empty() || !resume.sources.is_empty() {
        tracing::info!(
            lessons = resume.lessons.len(),
            videos = resume.videos.len(),
            sources = resume.sources.len(),
            "resuming queued work"
        );
    }
    Ok(resume)
}

/// Forgets expired logins and stale rate-limit windows.
async fn housekeeping(state: AppState) {
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    loop {
        tick.tick().await;
        auth::purge_expired_logins(&state);
        state.limiter.purge();
    }
}

/// Resolves on SIGINT or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
}
