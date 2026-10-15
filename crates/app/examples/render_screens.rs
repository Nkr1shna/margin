//! Render app screens to static HTML with the fake library, for visual checks without a
//! window (e.g. snapshotting in an offscreen webview on CI or a headless machine):
//!
//! ```sh
//! cargo run -p margin-app --example render_screens -- /lesson/l1 out.html [light|dark] [WIDTHpx]
//! cargo run -p margin-app --example render_screens -- connect out.html
//! cargo run -p margin-app --example render_screens -- /chat/c1 out.html dark
//! ```
//!
//! The sample chats are `c1` (an answered conversation with a tool call and passages),
//! `c2` (interrupted) and `c3` (on Claude: a web search and a cited answer); `/` is a
//! new chat.
//!
//! Optional environment, for screens that open over the routes:
//! - `RENDER_SETTINGS=<pane>` opens Settings at a pane (`sources`, `ai`, …).
//! - `RENDER_SOURCES=<topic id>` opens that topic's Sources dialog.
//! - `RENDER_UPLOADS=1` adds sample files and a folder uploading to the topic of
//!   `RENDER_SOURCES`.
//! - `RENDER_OUTLINE=ready|created` adds a proposed course's outline card to chat `c1`
//!   (`created`: made into the Fourier series topic).
//! - `RENDER_CONFIRM=pending|approved|denied|stopped|expired` adds a confirmation card to
//!   chat `c1` (`pending`: the answer waits on it).
//! - `RENDER_EMBEDDING=missing|downloading|ready|failed` sets the embedding model's state.
//! - `RENDER_VIEWER=<n>` opens the source viewer on the lesson's citation n (5: a
//!   Markdown note and 2: a script open on their cited lines in the file);
//!   `RENDER_VIEWER=folder` (or `folder-gone`) on a passage of a folder's PDF (or of a
//!   file whose folder was removed).
//! - `RENDER_PIPELINE=changed` sets OpenAI's three model slots, a variant's override of
//!   the small-tasks route and a changed pipeline budget (Settings → AI,
//!   `RENDER_SETTINGS=ai`; Reset to default is offered then), and the hybrid variant's
//!   stages with `take-k` packing ("Custom" in the variant picker). Settings → AI always
//!   shows the stage pickers, over the fake's variants, and each provider's models (with
//!   their windows) where it has a credential.
//!   `RENDER_PIPELINE=r2` sets the r2-full variant's stages (the Rewriter and the
//!   listwise reranker).
//! - `RENDER_RERANKER=missing|downloading|ready|failed` (with `RENDER_EMBEDDING`) adds the
//!   reranker model to Settings → Sources in that state; `ready` also makes the
//!   cross-encoder's variants available in Settings → AI.
//! - The sample lessons say what wrote them in their details line (`/lesson/l1`: Claude,
//!   `/lesson/l2`: OpenAI); `l3` failed, so it has none.
//! - `RENDER_PROVIDER=claude` makes Claude the default provider, with its key set (the new
//!   chat's picker preset to it; `RENDER_PROVIDER=claude-nokey` without the key, so the
//!   new chat says to set it up). Chat `c3` is on Claude, with a web search, its results
//!   card and an answer citing the pages.
//! - `RENDER_CHATGPT=waiting|signed_in|failed` sets where "Sign in with ChatGPT" is
//!   (Settings → AI); signed in, ChatGPT can be picked for a new chat.
//! - `RENDER_SETTINGS=advanced` shows trace retention.
//! - `RENDER_SETTINGS=organisation` shows people, invitations, groups and agents (and
//!   `account` the person, My settings and devices per person) as the owner;
//!   `RENDER_AS=member` signs in as Bob, a member, instead (Settings → AI, Voice & video
//!   and Advanced then show the organisation's values read-only).
//! - The sample folders (two devices', every sync state) are always there: Settings →
//!   Folders (`RENDER_SETTINGS=folders`) and chat `c1`'s strip show them.
//! - `RENDER_SOURCE_WARNING=1` shows the sample recording (s5) read without a transcript.
//! - `RENDER_PINNED=<url>` renders `connect` as a release build pinned to that service
//!   (`RENDER_PINNED=dev`: as a developer build, with the address field); otherwise it
//!   has this build's shape.
//! - `RENDER_JS=<script>` runs a script once the page has loaded (scrolling, opening a
//!   tooltip…), since effects don't run here.
//!
//! The Read view's lesson body is filled in on load, and the sample sources' media are
//! the files in `examples/sample-media`.
//!
//! Effects, futures and JS evals don't run in server-side rendering, so live parts
//! (selection, settings fetched over HTTP) show their initial state. Test tooling only.

#![allow(dead_code, unused_imports)]

#[path = "../src/api.rs"]
mod api;
#[path = "../src/citations.rs"]
mod citations;
#[path = "../src/commands.rs"]
mod commands;
#[path = "../src/config.rs"]
mod config;
#[path = "../src/drafts.rs"]
mod drafts;
#[path = "../src/fake.rs"]
mod fake;
#[path = "../src/folders/mod.rs"]
mod folders;
#[path = "../src/markdown.rs"]
mod markdown;
#[path = "../src/org.rs"]
mod org;
#[path = "../src/picker/mod.rs"]
mod picker;
#[path = "../src/routes.rs"]
mod routes;
#[path = "../src/secrets.rs"]
mod secrets;
#[path = "../src/sources.rs"]
mod sources;
#[path = "../src/state.rs"]
mod state;
#[path = "../src/store.rs"]
mod store;
#[path = "../src/tools/mod.rs"]
mod tools;
#[path = "../src/ui/mod.rs"]
mod ui;
#[path = "../src/zoom.rs"]
mod zoom;

use std::collections::HashSet;
use std::path::Path;
use std::rc::Rc;

use chrono::Utc;
use dioxus::history::{History, MemoryHistory};
use dioxus::prelude::*;
use margin_shared::{
    Device, EmbeddingModel, EmbeddingModelState, Me, ReasoningEffort, ReindexProgress, Settings,
    SourceKind, SourceStatus, SpeechModel,
};

use crate::api::Api;
use crate::state::{AppCtx, LiveStatus, LoadStatus, Session};
use crate::store::LibraryState;

#[derive(Props, Clone, PartialEq)]
struct HarnessProps {
    route: String,
}

#[allow(non_snake_case)]
fn Harness(props: HarnessProps) -> Element {
    let route = props.route.clone();
    use_context_provider(|| Rc::new(MemoryHistory::with_initial_path(route)) as Rc<dyn History>);
    let connect = props.route == "connect";
    let viewer_lesson = lesson_in_route(&props.route);
    use_context_provider(move || {
        let mut lib = LibraryState::from_library(sample_library());
        let chats = fake::sample_chats();
        lib.replace_chats(chats.iter().map(|(c, _)| c.clone()).collect());
        for (chat, mut items) in chats {
            fake::rebase_chat_media(&mut items, |path| {
                sample_media_url(path).unwrap_or_else(|| format!("http://127.0.0.1:9{path}"))
            });
            lib.chat_items.insert(chat.id, items);
        }
        if std::env::var_os("RENDER_SOURCE_WARNING").is_some()
            && let Some(s5) = lib.sources.get_mut("s5")
        {
            s5.status = SourceStatus::Ready {
                chunks: 2,
                pages: None,
                warning: Some(
                    "It wasn't transcribed: fluidaudiocli isn't installed on the service Mac \
                     (Settings → Tools shows how to install it). Its sound is still searched, \
                     but lessons can't quote what's said. Read it again once that's fixed."
                        .into(),
                ),
            };
        }
        lib.settings = Some(Settings {
            openai_base_url: "https://api.openai.com/v1".into(),
            openai_model: margin_shared::OPENAI_DEFAULT_MODEL.into(),
            has_api_key: true,
            voice: None,
            render_width: 1920,
            render_height: 1080,
            render_fps: 30,
            reasoning_effort: ReasoningEffort::Medium,
            agent_model: None,
            pipeline: Default::default(),
            trace_retention_days: margin_shared::TRACE_RETENTION_DAYS,
            default_provider: Default::default(),
            openai_models: margin_shared::RouteModels::openai_default(),
            chatgpt_models: margin_shared::RouteModels::chatgpt_default(),
            claude_models: margin_shared::RouteModels::claude_default(),
            has_claude_api_key: false,
            chatgpt: None,
        });
        if std::env::var("RENDER_PIPELINE").as_deref() == Ok("changed")
            && let Some(settings) = lib.settings.as_mut()
        {
            settings.openai_models = margin_shared::RouteModels {
                chat: "gpt-6.1-sol".into(),
                write: "gpt-6-astra".into(),
                small: "gpt-6-luna".into(),
            };
            settings.pipeline.routes.small = Some("gpt-6-luna".into());
            settings.pipeline.budget.share_of_window = 0.75;
            settings.pipeline.budget.images_max = 24;
            let variants = fake::fake_pipeline_variants();
            if let Some(hybrid) = variants.variants.iter().find(|v| v.name == "hybrid") {
                settings.pipeline = settings
                    .pipeline
                    .with_stages_of(&hybrid.config)
                    .with_stage("packer", "take-k");
            }
        }
        if let Some(settings) = lib.settings.as_mut() {
            match std::env::var("RENDER_PROVIDER").as_deref() {
                Ok("claude") => {
                    settings.default_provider = margin_shared::Provider::Claude;
                    settings.has_claude_api_key = true;
                }
                Ok("claude-nokey") => settings.default_provider = margin_shared::Provider::Claude,
                _ => {}
            }
            let status = match std::env::var("RENDER_CHATGPT").as_deref() {
                Ok("waiting") => Some(margin_shared::ChatGptStatus::Waiting {
                    authorize_url: "https://auth.openai.com/api/accounts/authorize?client_id=dynamic_agent_client&agent_name_hint=Margin&response_type=code&state=st-1&redirect_uri=http%3A%2F%2F127.0.0.1%3A1455%2Fauth%2Fcallback".into(),
                }),
                Ok("signed_in") => {
                    let account = fake::fake_account("owner@example.com");
                    settings.chatgpt = Some(account.clone());
                    Some(margin_shared::ChatGptStatus::SignedIn {
                        account,
                        last_error: None,
                    })
                }
                Ok("failed") => Some(margin_shared::ChatGptStatus::Failed {
                    message: "The sign-in was cancelled in the browser.".into(),
                }),
                _ => None,
            };
            let settings = settings.clone();
            lib.chatgpt = status;
            // Each provider's models, where it has a key or a sign-in (the fetch that
            // fills them doesn't run here).
            for p in margin_shared::Provider::ALL {
                if settings.has_credential(p) {
                    lib.provider_models.insert(p, fake::fake_models(p));
                }
            }
        }
        if std::env::var("RENDER_PIPELINE").as_deref() == Ok("r2")
            && let Some(settings) = lib.settings.as_mut()
        {
            let variants = fake::fake_pipeline_variants();
            if let Some(full) = variants.variants.iter().find(|v| v.name == "r2-full") {
                settings.pipeline = settings.pipeline.with_stages_of(&full.config);
            }
        }
        // The cross-encoder's variants are available once its model is downloaded.
        lib.pipeline_variants = Some(match std::env::var("RENDER_RERANKER").as_deref() {
            Ok("ready") => fake::pipeline_variants_with(Some(&EmbeddingModelState::Ready {
                bytes: fake::RERANKER_DOWNLOAD_BYTES,
            })),
            _ => fake::fake_pipeline_variants(),
        });
        // The text and code sources' originals, so the viewer shows their cited lines
        // (the fetch it would make doesn't run here).
        for source in lib.sources.values() {
            if let (Some(url), Some(text)) = (&source.media_url, fake::sample_original(&source.id))
            {
                ui::citations::remember_original(url, text);
            }
        }
        lib.embedding = std::env::var("RENDER_EMBEDDING")
            .ok()
            .map(|state| EmbeddingModel {
                repo: fake::EMBEDDING_REPO.into(),
                state: match state.as_str() {
                    "downloading" => EmbeddingModelState::Downloading {
                        received: 583_000_000,
                        total: fake::EMBEDDING_DOWNLOAD_BYTES,
                    },
                    "ready" => EmbeddingModelState::Ready {
                        bytes: fake::EMBEDDING_DOWNLOAD_BYTES,
                    },
                    "failed" => EmbeddingModelState::Failed {
                        error: "Hugging Face answered 503.".into(),
                    },
                    _ => EmbeddingModelState::NotDownloaded {
                        download_bytes: fake::EMBEDDING_DOWNLOAD_BYTES,
                    },
                },
                // A Linux service's speech model, and its re-index after a new embedder.
                speech: std::env::var_os("RENDER_SPEECH").map(|_| SpeechModel {
                    repo: fake::SPEECH_REPO.into(),
                    state: EmbeddingModelState::NotDownloaded {
                        download_bytes: fake::SPEECH_DOWNLOAD_BYTES,
                    },
                }),
                reindex: std::env::var_os("RENDER_REINDEX").map(|_| ReindexProgress {
                    done: 212,
                    total: 1480,
                }),
                // R2's reranker model (Settings → Sources).
                reranker: std::env::var("RENDER_RERANKER").ok().map(|state| {
                    margin_shared::RerankerModelState {
                        name: fake::RERANKER_NAME.into(),
                        repo: fake::RERANKER_REPO.into(),
                        state: match state.as_str() {
                            "downloading" => EmbeddingModelState::Downloading {
                                received: 9_400_000,
                                total: fake::RERANKER_DOWNLOAD_BYTES,
                            },
                            "ready" => EmbeddingModelState::Ready {
                                bytes: fake::RERANKER_DOWNLOAD_BYTES,
                            },
                            "failed" => EmbeddingModelState::Failed {
                                error: "Hugging Face answered 503.".into(),
                            },
                            _ => EmbeddingModelState::NotDownloaded {
                                download_bytes: fake::RERANKER_DOWNLOAD_BYTES,
                            },
                        },
                    }
                }),
            });
        let uploads = match std::env::var("RENDER_SOURCES") {
            Ok(topic_id) if std::env::var_os("RENDER_UPLOADS").is_some() => {
                sample_uploads(&mut lib, &topic_id)
            }
            _ => Vec::new(),
        };
        add_sample_cards(&mut lib);
        lib.replace_workspaces(fake::sample_workspaces());
        let mut folder_files = fake::sample_folder_files();
        for f in &mut folder_files {
            f.media_url = None;
        }
        lib.replace_folder_files("w1", folder_files);
        if let Some(chat) = lib.chats.get_mut("c1") {
            chat.workspace_ids = vec!["w1".into(), "w3".into(), "w4".into()];
        }
        let viewer = std::env::var("RENDER_VIEWER").ok();
        let shown_folder = viewer.as_deref().and_then(|v| {
            let n = match v {
                "folder" => 1,
                "folder-gone" => 3,
                _ => return None,
            };
            let mut c = fake::sample_folder_citations()[n].clone();
            c.image_url = c.image_url.as_deref().and_then(sample_media_url);
            Some(c)
        });
        let settings_open = std::env::var("RENDER_SETTINGS").ok().and_then(|name| {
            ui::settings::SettingsPane::ALL
                .into_iter()
                .find(|p| format!("{p:?}").eq_ignore_ascii_case(&name))
        });
        // The organisation: Alice (owner) and Bob (member), their groups, agents and
        // devices; RENDER_AS=member signs in as Bob, who sees only his own agents and
        // devices and no invitations.
        let member = std::env::var("RENDER_AS").as_deref() == Ok("member");
        let org = fake::FakeState::new();
        lib.replace_people(org.people.clone());
        lib.replace_groups(org.groups.clone());
        lib.replace_agents(org.agents.clone());
        lib.replace_invitations(org.invitations.clone());
        lib.replace_devices(org.devices.clone());
        if member {
            lib.agents.retain(|_, a| a.owner_id == fake::BOB);
            lib.invitations.clear();
            lib.devices
                .retain(|_, d| d.person_id.as_deref() == Some(fake::BOB));
        }
        let mut me = fake::sample_me(if member { fake::TOKEN_BOB } else { fake::TOKEN });
        if let Some(device) = me.device.as_mut()
            && !member
        {
            device.name = "Krishna's MacBook".into();
            device.last_seen_at = Some(Utc::now());
            device.online = true;
        }
        let session = (!connect).then(|| Session {
            api: Api::with_token("http://127.0.0.1:9", "x"),
            me: Some(me),
        });
        AppCtx {
            config: Signal::new(config::LocalConfig {
                service_url: Some("https://margin.krishnanelloore.com".into()),
                appearance: config::Appearance::System,
                sidebar_hidden: false,
                sidebar_width: None,
                zoom: 100,
            }),
            session: Signal::new(session),
            signed_out_reason: Signal::new(None),
            lib: Signal::new(lib),
            lib_status: Signal::new(LoadStatus::Loaded),
            reload: Signal::new(0),
            live: Signal::new(LiveStatus::Live),
            search_open: Signal::new(false),
            drawer_open: Signal::new(false),
            collapsed: Signal::new(HashSet::new()),
            outline_drafts: Signal::new(Default::default()),
            ask_prompt: Signal::new(String::new()),
            uploads: Signal::new(uploads),
            settings_open: Signal::new(settings_open),
            source_viewer: Signal::new(
                std::env::var("RENDER_VIEWER")
                    .ok()
                    .and_then(|n| n.parse().ok())
                    .zip(viewer_lesson)
                    .map(|(n, lesson_id)| state::SourceViewer { lesson_id, n }),
            ),
            transcripts: Signal::new(
                ["s5", "s6"]
                    .into_iter()
                    .map(|id| (id.to_string(), fake::sample_transcript(id)))
                    .collect(),
            ),
            shown_passage: Signal::new(shown_folder),
            device_frames: Signal::new(None),
            agent_commands: Signal::new(Default::default()),
            foreground: Signal::new(true),
            folders: Signal::new(folders::store::LocalFolders {
                held: [
                    ("w1".to_string(), folders::store::FolderStatus::Ok),
                    (
                        "w2".to_string(),
                        folders::store::FolderStatus::Revoked(
                            "macOS didn't let Margin read “Papers”.".into(),
                        ),
                    ),
                ]
                .into(),
            }),
            folder_sync: Signal::new(
                [(
                    "w1".to_string(),
                    folders::sync::SyncState::Uploading {
                        done: 2,
                        total: 12,
                        bytes_done: 41 * 1024 * 1024,
                        bytes_total: 230 * 1024 * 1024,
                    },
                )]
                .into(),
            ),
            ask_workspaces: Signal::new(Vec::new()),
        }
    });
    let sources_topic = std::env::var("RENDER_SOURCES").ok();
    if connect {
        // The build's shape unless RENDER_PINNED names a service (or `dev` for a
        // developer build).
        let pinned = match std::env::var("RENDER_PINNED").ok().as_deref() {
            None => config::PINNED_SERVICE_URL.map(String::from),
            Some("dev") => None,
            Some(url) => Some(url.to_string()),
        };
        rsx! { ui::signin::ConnectScreen { pinned } }
    } else {
        rsx! {
            Router::<routes::Route> {}
            if let Some(topic_id) = sources_topic {
                ui::sources::TopicSourcesDialog { topic_id, on_close: |_| {} }
            }
        }
    }
}

/// The cards `RENDER_OUTLINE` and `RENDER_CONFIRM` ask for, at the end of chat `c1`.
fn add_sample_cards(lib: &mut LibraryState) {
    use margin_shared::{Card, ChatState, ConfirmationCard, Decision, OutlineCard};
    let mut cards = Vec::new();
    if let Ok(outline) = std::env::var("RENDER_OUTLINE") {
        let created = (outline == "created").then_some("fourier");
        let proposal = fake::sample_proposal("c1", created);
        cards.push(fake::sample_card(
            "c1",
            20,
            Card::Outline(OutlineCard {
                proposal_id: proposal.id.clone(),
            }),
        ));
        lib.proposals.insert(proposal.id.clone(), proposal);
    }
    if let Ok(confirm) = std::env::var("RENDER_CONFIRM") {
        let decision = match confirm.as_str() {
            "approved" => Decision::Approved {
                device_id: "d1".into(),
            },
            "denied" => Decision::Denied {
                device_id: "d2".into(),
                by_stop: false,
            },
            "stopped" => Decision::Denied {
                device_id: "d1".into(),
                by_stop: true,
            },
            "expired" => Decision::Expired,
            _ => Decision::Pending,
        };
        let card = fake::sample_card(
            "c1",
            21,
            Card::Confirmation(ConfirmationCard {
                tool: "delete_note".into(),
                title: "Delete the note “Questions for Friday”?".into(),
                detail: "The note and its 2 paragraphs are deleted. This can't be undone.".into(),
                approve_label: "Delete".into(),
                decision: decision.clone(),
            }),
        );
        if decision == Decision::Pending
            && let Some(chat) = lib.chats.get_mut("c1")
        {
            chat.state = ChatState::WaitingForUser {
                turn_id: "t2".into(),
                device_id: "d1".into(),
                item_id: card.id.clone(),
            };
        }
        cards.push(card);
    }
    if let Some(items) = lib.chat_items.get_mut("c1") {
        items.extend(cards);
    }
}

/// Files uploading to `topic_id`, in each state, with their sources in the store.
fn sample_uploads(lib: &mut LibraryState, topic_id: &str) -> Vec<sources::Upload> {
    use sources::{Candidate, Upload, UploadState};
    let file = |name: &str, folder: Option<&str>, bytes| {
        Upload::new(
            Candidate {
                path: format!("/tmp/{name}").into(),
                name: name.into(),
                folder: folder.map(Into::into),
                kind: SourceKind::for_name(name).unwrap(),
                bytes,
            },
            Some(topic_id.to_string()),
        )
    };
    let done = |mut upload: Upload, id: &str, status: SourceStatus, lib: &mut LibraryState| {
        let source = margin_shared::Source {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            media_url: None,
            id: id.into(),
            topic_id: Some(topic_id.to_string()),
            name: upload.name.clone(),
            folder: upload.folder.clone(),
            kind: upload.kind,
            bytes: upload.bytes,
            status,
            created_at: Utc::now(),
            workspace_id: None,
            modified: None,
        };
        lib.sources.insert(source.id.clone(), source.clone());
        upload.state = UploadState::Done(Box::new(source));
        upload
    };
    let mut failed = file("scan.pdf", None, 9_800_000);
    failed.state = UploadState::Failed("The service refused the request (413).".into());
    let mut uploading = file("lecture-notes/week2.md", Some("course"), 12_400);
    uploading.state = UploadState::Uploading;
    vec![
        done(
            file("Signals and Systems, ch. 3.pdf", None, 4_100_000),
            "u1",
            SourceStatus::Processing {
                done: 12,
                total: 40,
            },
            lib,
        ),
        failed,
        done(
            file("lecture-notes/week1.md", Some("course"), 8_100),
            "u2",
            SourceStatus::Ready {
                chunks: 3,
                pages: None,
                warning: None,
            },
            lib,
        ),
        uploading,
        file("src/fourier.py", Some("course"), 3_300),
    ]
}

fn file_url(p: &Path) -> String {
    format!("file://{}", p.display())
}

/// A sample source's media file (`/media/sources/{id}/{file}…`) as a local file URL.
fn sample_media_url(path: &str) -> Option<String> {
    let mut parts = path.split('?').next().unwrap_or(path).split('/').skip(3);
    let (id, file) = (parts.next()?, parts.next()?);
    fake::sample_media_file(id, file).map(|p| file_url(&p))
}

/// The fake library with its sample media as local files.
fn sample_library() -> margin_shared::Library {
    let mut library = fake::sample_library();
    fake::rebase_media(&mut library, |path| {
        if path.split('/').count() < 5 {
            return path.to_string();
        }
        sample_media_url(path).unwrap_or_else(|| format!("http://127.0.0.1:9{path}"))
    });
    library
}

/// The lesson a `/lesson/{id}…` route shows.
fn lesson_in_route(route: &str) -> Option<String> {
    route
        .strip_prefix("/lesson/")
        .map(|rest| rest.split('/').next().unwrap_or(rest).to_string())
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let route = args.next().unwrap_or_else(|| "/".into());
    let out = args.next().unwrap_or_else(|| "screen.html".into());
    let theme = args.next().unwrap_or_else(|| "light".into());

    // The Read view puts the lesson body into the page from an effect, which server-side
    // rendering doesn't run; the snapshot page does it on load instead.
    let body_script = lesson_in_route(&route)
        .filter(|_| route.matches('/').count() == 2)
        .and_then(|id| {
            let lesson = sample_library().lessons.into_iter().find(|l| l.id == id)?;
            let html =
                markdown::render_lesson(&lesson.body_md, &citations::ordered(&lesson.citations));
            Some(format!(
                "MarginApp.setHtml('lesson-body', {});",
                serde_json::to_string(&html).unwrap()
            ))
        })
        .unwrap_or_default();
    let mut dom = VirtualDom::new_with_props(Harness, HarnessProps { route });
    dom.rebuild_in_place();
    let body = dioxus_ssr::render(&dom);

    let assets = Path::new(env!("CARGO_MANIFEST_DIR")).join("assets");
    let fonts = assets.join("fonts");
    let html = format!(
        r#"<!doctype html>
<html data-theme="{theme}"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
@font-face {{ font-family: 'Albert Sans'; font-weight: 100 900; src: url('{f1}') format('truetype'); }}
@font-face {{ font-family: 'Albert Sans'; font-style: italic; font-weight: 100 900; src: url('{f2}') format('truetype'); }}
</style>
<link rel="stylesheet" href="{css}">
<link rel="stylesheet" href="{kcss}">
<script src="{kjs}"></script>
<script src="{mjs}"></script>
</head><body><div id="main">{body}</div>
<script>window.addEventListener('load', function () {{ {body_script} MarginApp.renderMathIn('.md'); {extra_js} }});</script>
</body></html>"#,
        f1 = file_url(&fonts.join("AlbertSans-wght.ttf")),
        f2 = file_url(&fonts.join("AlbertSans-Italic-wght.ttf")),
        css = file_url(&assets.join("main.css")),
        kcss = file_url(&assets.join("katex/katex.min.css")),
        kjs = file_url(&assets.join("katex/katex.min.js")),
        mjs = file_url(&assets.join("margin.js")),
        extra_js = std::env::var("RENDER_JS").unwrap_or_default(),
    );
    std::fs::write(&out, html).expect("write html");
    println!("wrote {out}");
}
