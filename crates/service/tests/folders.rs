//! Phase 4, service side (`docs/agent-phase4-brief.md` D13): attached folders' files
//! synced to the service by manifest, uploaded as folder sources, read by the one
//! ingest pipeline, listed and searched by the agent, cited in lessons, re-synced,
//! scoped out of the library and deleted with their folder.
//!
//! Through the real router with the harness's fakes: `FakeSourceTools` reads a "PDF" as
//! pages split by form feeds (each rendered to a fake picture) and an "image" as its own
//! picture; the scripted model plays the agent. Manifests carry real SHA-256 stamps of
//! the test bytes.
//!
//! The last two need the 4-agent area's `margin_agent` (`sync_folder`,
//! `ToolOutput.images`): `sync_folder` in `tool_choice` and its device round trip
//! (D13 test 3), and `view_picture`'s `input_image` item in the stored `model_items`
//! and the next request (the pixels part of test 1).

mod common;

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use common::*;
use margin_agent::scripted::{text_reply, tool_calls_reply};
use margin_agent::{NoDevices, Scripted, ToolContext};
use margin_service::ask::StoredProposal;
use margin_service::db;
use margin_service::folders::FolderFileRow;
use margin_service::state::StoredLesson;
use margin_shared::context::{ContextChanges, Document, Origin, Scope};
use margin_shared::routes::{self, UPLOAD_PART_BYTES};
use margin_shared::{
    ApiError, Card, Chat, ChatDetail, ChatItemKind, ChatState, Citation, DeviceRef, Event,
    FileStamp, Lesson, LessonStatus, Library, ManifestFile, Source, SourceKind, SourceStatus,
    SourceUpload, SyncManifest, SyncPlan, Workspace, WorkspaceFile, folder_file_type,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn t0() -> DateTime<Utc> {
    DateTime::from_timestamp(1_760_000_000, 0).unwrap()
}

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A manifest entry for `path` holding `bytes`, modified at `t0`.
fn listed(path: &str, bytes: &[u8]) -> ManifestFile {
    ManifestFile {
        path: path.into(),
        kind: folder_file_type(path).unwrap(),
        stamp: FileStamp {
            bytes: bytes.len() as u64,
            modified: Some(t0()),
            sha256: sha(bytes),
        },
    }
}

async fn api_error(r: reqwest::Response) -> (u16, ApiError) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

async fn library(h: &Harness, t: &str) -> Library {
    let r = h.get(t, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// `POST /api/workspaces` from the device of `t`.
async fn new_folder(h: &Harness, t: &str, name: &str) -> Workspace {
    let r = h
        .post(t, "/api/workspaces")
        .json(&json!({"name": name, "platform": "macos", "location": format!("~/{name}")}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

async fn sync_raw(h: &Harness, t: &str, ws: &str, files: Vec<ManifestFile>) -> reqwest::Response {
    h.post(t, &routes::workspace_sync(ws))
        .json(&SyncManifest {
            files,
            unreadable: vec![],
        })
        .send()
        .await
        .unwrap()
}

async fn sync(h: &Harness, t: &str, ws: &str, files: Vec<ManifestFile>) -> SyncPlan {
    let r = sync_raw(h, t, ws, files).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    r.json().await.unwrap()
}

/// Uploads a folder's file: one request up to `UPLOAD_PART_BYTES`, else (or when
/// `in_parts`) through `SOURCE_UPLOADS`.
async fn upload_file(
    h: &Harness,
    t: &str,
    ws: &str,
    path: &str,
    bytes: &[u8],
    in_parts: bool,
) -> reqwest::Response {
    let modified = t0().to_rfc3339();
    if !in_parts && bytes.len() as u64 <= UPLOAD_PART_BYTES {
        return h
            .post(t, routes::SOURCES)
            .query(&[
                ("name", path),
                ("workspace_id", ws),
                ("modified", &modified),
            ])
            .body(bytes.to_vec())
            .send()
            .await
            .unwrap();
    }
    let r = h
        .post(t, routes::SOURCE_UPLOADS)
        .query(&[
            ("name", path),
            ("workspace_id", ws),
            ("modified", &modified),
            ("bytes", &bytes.len().to_string()),
        ])
        .send()
        .await
        .unwrap();
    if r.status() != 200 {
        return r;
    }
    let up: SourceUpload = r.json().await.unwrap();
    // Small parts, to go through several even for a small file.
    let part = (bytes.len() / 3).clamp(1, UPLOAD_PART_BYTES as usize);
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + part).min(bytes.len());
        let r = h
            .put(t, &routes::source_upload(&up.id))
            .query(&[("offset", offset)])
            .body(bytes[offset..end].to_vec())
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
        offset = end;
    }
    h.post(t, &routes::source_upload_finish(&up.id))
        .send()
        .await
        .unwrap()
}

async fn upload_ok(h: &Harness, t: &str, ws: &str, path: &str, bytes: &[u8]) -> Source {
    let r = upload_file(h, t, ws, path, bytes, false).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    r.json().await.unwrap()
}

async fn workspace(h: &Harness, t: &str, id: &str) -> Option<Workspace> {
    let all: Vec<Workspace> = h
        .get(t, "/api/workspaces")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    all.into_iter().find(|w| w.id == id)
}

async fn files(h: &Harness, t: &str, ws: &str) -> Vec<Source> {
    let r = h.get(t, &routes::workspace_files(ws)).send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// Waits until every file of the folder is read and the folder's counts say so.
async fn folder_read(h: &Harness, t: &str, ws: &str, n: u32) -> Workspace {
    eventually(|| async {
        let w = workspace(h, t, ws).await?;
        let s = &w.sync;
        (s.files == n && s.uploading == 0 && s.reading == 0).then_some(w)
    })
    .await
}

async fn rows(h: &Harness, ws: &str) -> Vec<FolderFileRow> {
    h.state
        .store
        .list_where(db::FOLDER_FILE, "workspace_id", ws)
        .await
        .unwrap()
}

async fn document(h: &Harness, id: &str) -> Option<Document> {
    margin_service::ingest::document(&h.state, id)
        .await
        .unwrap()
}

async fn chat_detail(h: &Harness, t: &str, id: &str) -> ChatDetail {
    h.get(t, &format!("/api/chats/{id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap()
}

async fn wait_idle(h: &Harness, t: &str, id: &str, items_at_least: usize) -> ChatDetail {
    eventually(|| async {
        let d = chat_detail(h, t, id).await;
        (d.chat.state == ChatState::Idle && d.items.len() >= items_at_least).then_some(d)
    })
    .await
}

/// The output the model got for `call_id`.
fn output_sent(h: &Harness, call_id: &str) -> Value {
    let text = h
        .generator
        .agent
        .requests()
        .iter()
        .flat_map(|r| r.input.iter())
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == call_id)
        .and_then(|i| i["output"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no output for {call_id}"));
    serde_json::from_str(&text).unwrap_or(Value::String(text))
}

/// The tools a request lets the model call: `allowed_tools` when it restricts them,
/// else every tool in `tools`. Hosted tools go by their type (`"web_search"`), as
/// `allowed_tools` names them.
fn offered(req: &margin_ai::ResponseRequest) -> Vec<String> {
    match &req.tool_choice {
        Some(margin_ai::ResponsesToolChoice::AllowedTools { names, .. }) => names.clone(),
        Some(margin_ai::ResponsesToolChoice::Auto) | None => req
            .tools
            .iter()
            .filter_map(|t| {
                t["name"]
                    .as_str()
                    .or_else(|| {
                        t["type"]
                            .as_str()
                            .filter(|ty| margin_ai::HOSTED_TOOLS.contains(ty))
                    })
                    .map(str::to_string)
            })
            .collect(),
        other => panic!("tool_choice {other:?}"),
    }
}

fn tool_ctx(chat_id: &str, workspaces: Vec<Workspace>) -> ToolContext {
    ToolContext {
        chat_id: chat_id.into(),
        turn_id: "turn".into(),
        call_id: "call".into(),
        origin_device: DeviceRef {
            device_id: "d".into(),
            name: "Mac".into(),
        },
        workspaces,
        devices: std::sync::Arc::new(NoDevices),
    }
}

/// Runs a service tool directly.
async fn run_tool(
    h: &Harness,
    ctx: &ToolContext,
    name: &str,
    args: Value,
) -> margin_agent::ToolOutput {
    let registry = margin_service::agent_tools::registry(&h.state);
    let tool = registry
        .get(name)
        .unwrap_or_else(|| panic!("no tool {name}"));
    tool.run(ctx, args).await
}

const INTRO: &str = "# Intro\n\nPlants make sugar from light.\n\nThe overview of chlorophyll.";
const FIGURE: &str = "PICTURE a green leaf in sunlight";

/// A twelve-page "PDF": page 3 is about photosynthesis, page 4 is empty.
fn paper() -> String {
    (1..=12)
        .map(|n| match n {
            3 => "Photosynthesis in the light reactions splits water.".to_string(),
            4 => String::new(),
            n => format!("Page {n} discusses topic number {n} at length."),
        })
        .collect::<Vec<_>>()
        .join("\x0c")
}

/// Signed in on a Mac with AI configured, a folder "notes" holding intro.md, paper.pdf
/// (sent in parts) and figure.png, all read.
async fn synced_notes() -> (Harness, String, Workspace) {
    let h = start().await;
    let t = h.sign_in("MacBook Pro").await;
    h.configure_ai(&t).await;
    let ws = new_folder(&h, &t, "notes").await;
    let paper = paper();
    let manifest = vec![
        listed("intro.md", INTRO.as_bytes()),
        listed("paper.pdf", paper.as_bytes()),
        listed("figure.png", FIGURE.as_bytes()),
    ];
    let plan = sync(&h, &t, &ws.id, manifest).await;
    assert_eq!(
        plan,
        SyncPlan {
            upload: vec!["figure.png".into(), "intro.md".into(), "paper.pdf".into()],
            kept: 0,
            removed: 0,
        }
    );
    upload_ok(&h, &t, &ws.id, "intro.md", INTRO.as_bytes()).await;
    let r = upload_file(&h, &t, &ws.id, "paper.pdf", paper.as_bytes(), true).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    upload_ok(&h, &t, &ws.id, "figure.png", FIGURE.as_bytes()).await;
    let ws = folder_read(&h, &t, &ws.id, 3).await;
    (h, t, ws)
}

fn by_name<'a>(files: &'a [Source], name: &str) -> &'a Source {
    files
        .iter()
        .find(|s| s.name == name)
        .unwrap_or_else(|| panic!("no {name} in {files:?}"))
}

// ---------------------------------------------------------------------------
// 1. Attach, sync, search, cite
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_folder_syncs_is_searched_listed_and_cited_in_a_course() {
    let h = start().await;
    let t = h.sign_in("MacBook Pro").await;
    h.configure_ai(&t).await;
    let mut socket = h.socket(&t).await;
    let ws = new_folder(&h, &t, "notes").await;
    let paper = paper();
    let plan = sync(
        &h,
        &t,
        &ws.id,
        vec![
            listed("intro.md", INTRO.as_bytes()),
            listed("paper.pdf", paper.as_bytes()),
            listed("figure.png", FIGURE.as_bytes()),
        ],
    )
    .await;
    assert_eq!(plan.upload, ["figure.png", "intro.md", "paper.pdf"]);
    assert_eq!((plan.kept, plan.removed), (0, 0));
    // The folder's counts went out: three to send, none there yet.
    let events = socket
        .until(|e| matches!(e, Event::WorkspaceChanged { workspace } if workspace.sync.last_synced_at.is_some()))
        .await;
    let Some(Event::WorkspaceChanged { workspace: changed }) = events.last() else {
        unreachable!()
    };
    assert_eq!((changed.sync.uploading, changed.sync.files), (3, 0));

    upload_ok(&h, &t, &ws.id, "intro.md", INTRO.as_bytes()).await;
    let r = upload_file(&h, &t, &ws.id, "paper.pdf", paper.as_bytes(), true).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let pdf: Source = r.json().await.unwrap();
    upload_ok(&h, &t, &ws.id, "figure.png", FIGURE.as_bytes()).await;
    assert_eq!(pdf.workspace_id.as_deref(), Some(ws.id.as_str()));
    assert_eq!(pdf.topic_id, None);
    assert_eq!(pdf.folder.as_deref(), Some("notes"));
    assert_eq!(pdf.kind, SourceKind::Pdf);
    assert_eq!(pdf.modified, Some(t0()));
    let w = folder_read(&h, &t, &ws.id, 3).await;
    assert_eq!(w.sync.failed, 0);
    assert_eq!(
        w.sync.bytes,
        (INTRO.len() + paper.len() + FIGURE.len()) as u64
    );
    let doc = document(&h, &pdf.id).await.unwrap();
    assert_eq!(
        doc.origin,
        Origin::FolderFile {
            workspace_id: ws.id.clone(),
            path: "paper.pdf".into()
        }
    );
    // The folder's files, with their media; never in the library.
    let listed_files = files(&h, &t, &ws.id).await;
    assert_eq!(listed_files.len(), 3);
    assert!(listed_files.iter().all(|s| s.media_url.is_some()));
    assert!(matches!(
        by_name(&listed_files, "paper.pdf").status,
        SourceStatus::Ready {
            pages: Some(12),
            ..
        }
    ));
    let lib = library(&h, &t).await;
    assert!(lib.sources.is_empty(), "{:?}", lib.sources);
    // The rows record what the service received, hashed by the service.
    let r = rows(&h, &ws.id).await;
    assert_eq!(r.len(), 3);
    assert!(r.iter().all(|r| r.received.as_ref() == Some(&r.listed)));

    // A chat with the folder attached: its tools.
    let fig = by_name(&listed_files, "figure.png").clone();
    let agent = &h.generator.agent;
    agent.push(Scripted::Events(tool_calls_reply(&[
        (
            "call_list",
            "list_files",
            &json!({"workspace_id": ws.id, "path": ""}).to_string(),
        ),
        (
            "call_find",
            "find_files",
            &json!({"workspace_id": ws.id, "path": null, "pattern": "**/*.pdf"}).to_string(),
        ),
        (
            "call_search",
            "search",
            &json!({
                "query": "photosynthesis light reactions",
                "scope": {"topic_id": null, "source_ids": null, "workspace_ids": [ws.id], "chat_files": null, "library": null},
                "k": null,
                "modalities": null,
            })
            .to_string(),
        ),
        (
            "call_view",
            "view_picture",
            &json!({"document_id": pdf.id, "page": 3}).to_string(),
        ),
        (
            "call_other",
            "list_files",
            &json!({"workspace_id": "nope", "path": null}).to_string(),
        ),
    ])));
    agent.push(Scripted::Events(text_reply(&["Found it."])));
    h.title_reply("Notes");
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"workspace_ids": [ws.id], "message": {"text": "What is in my notes?"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let chat: ChatDetail = r.json().await.unwrap();
    let d = wait_idle(&h, &t, &chat.chat.id, 3).await;

    // The folder tools are offered with a folder attached; a chat without one gets
    // search but not them.
    let requests = agent.requests();
    let first = offered(&requests[0]);
    for name in ["list_files", "find_files", "search", "view_picture"] {
        assert!(first.iter().any(|n| n == name), "{name} missing: {first:?}");
    }

    // list_files: the three files with their documents.
    let out = output_sent(&h, "call_list");
    assert_eq!(out["folder"], "notes");
    let entries = out["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 3, "{out}");
    let paper_entry = entries.iter().find(|e| e["path"] == "paper.pdf").unwrap();
    assert_eq!(paper_entry["document_id"], pdf.id);
    assert_eq!(paper_entry["kind"], "pdf");
    assert_eq!(paper_entry["pages"], 12);
    assert_eq!(paper_entry["status"], "read");
    assert_eq!(out["truncated"], false);
    assert_eq!(out["sync"]["device"], "MacBook Pro");
    // find_files: one PDF.
    let out = output_sent(&h, "call_find");
    let found: Vec<&str> = out["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(found, ["paper.pdf"]);
    // search: page 3 of the paper, with its picture and the folder file named.
    let out = output_sent(&h, "call_search");
    let passages = out["passages"].as_array().unwrap();
    let hit = passages
        .iter()
        .find(|p| p["document_id"] == pdf.id && p["page"] == 3)
        .unwrap_or_else(|| panic!("no page-3 hit: {out}"));
    assert_eq!(hit["source"], "notes/paper.pdf");
    assert_eq!(hit["picture"], true);
    let card = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                call_id,
                card: Card::Passages(c),
            } if call_id == "call_search" => Some(c.clone()),
            _ => None,
        })
        .expect("a passages card");
    let cited = card
        .passages
        .iter()
        .find(|c| c.source_id == pdf.id && c.page == Some(3))
        .expect("page 3 in the card");
    assert_eq!(
        cited.file,
        Some(WorkspaceFile {
            workspace_id: ws.id.clone(),
            path: "paper.pdf".into()
        })
    );
    assert!(
        cited
            .image_url
            .as_deref()
            .is_some_and(|u| u.contains("/media/sources/") && u.contains("sig=")),
        "{cited:?}"
    );
    // view_picture: the stored page, as text and a card.
    let out = output_sent(&h, "call_view");
    assert_eq!(out["source"], "notes/paper.pdf");
    assert_eq!(out["page"], 3);
    // A folder that isn't attached: the check's words.
    let out = output_sent(&h, "call_other");
    let msg = out.to_string();
    assert!(
        msg.contains(
            "No folder attached to this chat has the workspace_id “nope”. Attached: “notes”"
        ),
        "{msg}"
    );
    // Nothing went to the device.
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(200),
            socket.next_directed()
        )
        .await
        .is_err(),
        "a request reached the device"
    );

    // A chat without a folder is offered none of the folder tools.
    agent.push(Scripted::Events(text_reply(&["Hi."])));
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"message": {"text": "Hello"}}))
        .send()
        .await
        .unwrap();
    let plain: ChatDetail = r.json().await.unwrap();
    wait_idle(&h, &t, &plain.chat.id, 2).await;
    let n = agent.requests().len() - 1;
    let tools = offered(&agent.requests()[n]);
    assert!(tools.iter().any(|n| n == "search"));
    assert!(tools.iter().any(|n| n == "web_search"), "{tools:?}");
    for name in ["list_files", "find_files", "sync_folder"] {
        assert!(
            !tools.iter().any(|n| n == name),
            "{name} offered: {tools:?}"
        );
    }

    // Turn 2: a course in the chat draws on the folder.
    let req = margin_shared::AskRequest {
        prompt: "Photosynthesis light reactions".into(),
        lesson_count: 1,
        level: margin_shared::Level::Intro,
        with_videos: false,
        with_quiz: false,
        parent_topic_id: None,
        source_ids: vec![],
    };
    agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_propose",
        "propose_course",
        &propose_args(&req).to_string(),
    )])));
    agent.push(Scripted::Events(text_reply(&["Here is a course."])));
    let r = h
        .post(&t, &format!("/api/chats/{}/messages", chat.chat.id))
        .json(&json!({"text": "Make me a course from it"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let d = eventually(|| async {
        let d = chat_detail(&h, &t, &chat.chat.id).await;
        let carded = d.items.iter().any(|i| {
            matches!(
                &i.kind,
                ChatItemKind::Card {
                    card: Card::Outline(_),
                    ..
                }
            )
        });
        (d.chat.state == ChatState::Idle && carded).then_some(d)
    })
    .await;
    let proposal_id = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::Outline(c),
                ..
            } => Some(c.proposal_id.clone()),
            _ => None,
        })
        .unwrap();
    let stored: StoredProposal = h
        .state
        .store
        .get(db::PROPOSAL, &proposal_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.workspace_ids, std::slice::from_ref(&ws.id));
    let out = output_sent(&h, "call_propose");
    assert_eq!(out["draws_on_folders"], json!(["notes (MacBook Pro)"]));

    *h.generator.lesson_body.lock().unwrap() = Some("Intro [^1] [^2].".into());
    let r = h
        .post(&t, &format!("/api/proposals/{proposal_id}/accept"))
        .json(&json!({"outline": stored.proposal.outline.clone().unwrap()}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let lesson = eventually(|| async {
        let lib = library(&h, &t).await;
        lib.lessons
            .into_iter()
            .find(|l| l.status == LessonStatus::Ready)
    })
    .await;
    // Accepting moved no folder file into the topic.
    assert!(
        files(&h, &t, &ws.id)
            .await
            .iter()
            .all(|s| s.topic_id.is_none())
    );
    let folder_cites: Vec<&Citation> = lesson
        .citations
        .iter()
        .filter(|c| c.file.is_some())
        .collect();
    assert!(!folder_cites.is_empty(), "{:?}", lesson.citations);
    let page = lesson
        .citations
        .iter()
        .find(|c| c.source_id == pdf.id && c.page.is_some())
        .unwrap_or_else(|| panic!("no page of the paper cited: {:?}", lesson.citations));
    assert_eq!(page.source_name, "notes/paper.pdf");
    assert_eq!(page.kind, SourceKind::Pdf);
    assert_eq!(page.document_id.as_deref(), Some(pdf.id.as_str()));
    assert_eq!(
        page.file,
        Some(WorkspaceFile {
            workspace_id: ws.id.clone(),
            path: "paper.pdf".into()
        })
    );
    let url = page.image_url.clone().expect("a signed picture");
    assert_eq!(h.http.get(&url).send().await.unwrap().status(), 200);
    let stored_lesson: StoredLesson = h
        .state
        .store
        .get(db::LESSON, &lesson.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored_lesson.workspace_ids, std::slice::from_ref(&ws.id));

    // Regenerated with the device gone: from the stored passages.
    socket.close().await;
    let r = h
        .post(&t, &format!("/api/lessons/{}/regenerate", lesson.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let again: Lesson = eventually(|| async {
        let lib = library(&h, &t).await;
        lib.lessons.into_iter().find(|l| {
            l.id == lesson.id && l.status == LessonStatus::Ready && l.updated_at > lesson.updated_at
        })
    })
    .await;
    assert!(
        again
            .citations
            .iter()
            .any(|c| c.source_id == pdf.id && c.file.is_some())
    );
    let _ = fig;
}

// ---------------------------------------------------------------------------
// 2. Re-sync
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_resync_replaces_changed_files_in_place_and_removes_deleted_ones() {
    let (h, t, ws) = synced_notes().await;
    let all = files(&h, &t, &ws.id).await;
    let intro = by_name(&all, "intro.md").clone();
    let fig = by_name(&all, "figure.png").clone();
    let paper = paper();
    let intro_rows = h.state.store.passages_of(&intro.id).await.unwrap();
    let kept_row = intro_rows
        .iter()
        .find(|r| r.text.contains("Plants make sugar"))
        .unwrap()
        .clone();
    let changed_row = intro_rows
        .iter()
        .find(|r| r.text.contains("chlorophyll"))
        .unwrap()
        .clone();
    let generation = document(&h, &intro.id).await.unwrap().generation;
    // A lesson that cited the figure.
    let topic: margin_shared::Topic = h
        .post(&t, "/api/topics")
        .json(&json!({"title": "Plants"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let now = Utc::now();
    let cited = StoredLesson {
        lesson: Lesson {
            // Filed under the topic: its owner and audience (D6).
            access: topic.access.clone(),
            deny: Vec::new(),
            owner: topic.owner.clone(),
            id: "l-fig".into(),
            topic_id: topic.id.clone(),
            title: "Leaves".into(),
            summary: String::new(),
            position: 0,
            level: margin_shared::Level::Intro,
            minutes: 5,
            body_md: "A leaf [^1].".into(),
            status: LessonStatus::Ready,
            created_at: now,
            updated_at: now,
            citations: vec![Citation {
                n: 1,
                source_id: fig.id.clone(),
                source_name: "notes/figure.png".into(),
                kind: SourceKind::Image,
                page: None,
                start_ms: None,
                end_ms: None,
                excerpt: "a green leaf".into(),
                image_url: Some("original".into()),
                document_id: Some(fig.id.clone()),
                start_line: None,
                end_line: None,
                file: Some(WorkspaceFile {
                    workspace_id: ws.id.clone(),
                    path: "figure.png".into(),
                }),
                section_path: None,
            }],
            written_with: None,
        },
        proposal_id: None,
        previous_body: None,
        previous_citations: None,
        author: Default::default(),
        prompt: None,
        with_quiz: false,
        workspace_ids: vec![ws.id.clone()],
    };
    h.state
        .store
        .put(db::LESSON, "l-fig", &cited)
        .await
        .unwrap();
    let fig_url = library(&h, &t).await.lessons[0].citations[0]
        .image_url
        .clone()
        .unwrap();
    assert_eq!(h.http.get(&fig_url).send().await.unwrap().status(), 200);

    let mut socket = h.socket(&t).await;
    let intro2 = "# Intro\n\nPlants make sugar from light.\n\nStomata let carbon dioxide in.";
    let manifest = vec![
        listed("intro.md", intro2.as_bytes()),
        listed("paper.pdf", paper.as_bytes()),
        listed("new.md", b"A new note about roots."),
    ];
    let plan = sync(&h, &t, &ws.id, manifest.clone()).await;
    assert_eq!(
        plan,
        SyncPlan {
            upload: vec!["intro.md".into(), "new.md".into()],
            kept: 1,
            removed: 1,
        }
    );
    socket
        .until(|e| matches!(e, Event::SourceDeleted { id } if *id == fig.id))
        .await;
    assert!(h.state.store.passages_of(&fig.id).await.unwrap().is_empty());
    assert!(document(&h, &fig.id).await.is_none());
    let changes: ContextChanges = h
        .get(&t, routes::CONTEXT_CHANGES)
        .query(&[("since", "0")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(changes.deleted.contains(&fig.id));
    // The changed file is still searchable until its new copy lands.
    assert!(
        !h.state
            .store
            .passages_of(&intro.id)
            .await
            .unwrap()
            .is_empty()
    );
    // The lesson keeps its citation; the picture is gone.
    let lesson = library(&h, &t).await.lessons.remove(0);
    assert_eq!(lesson.citations[0].excerpt, "a green leaf");
    let url = lesson.citations[0].image_url.clone().unwrap();
    assert_eq!(h.http.get(&url).send().await.unwrap().status(), 404);

    // The new copy replaces the old one in place.
    let replaced = upload_ok(&h, &t, &ws.id, "intro.md", intro2.as_bytes()).await;
    assert_eq!(replaced.id, intro.id);
    upload_ok(&h, &t, &ws.id, "new.md", b"A new note about roots.").await;
    folder_read(&h, &t, &ws.id, 3).await;
    let doc = eventually(|| async {
        let d = document(&h, &intro.id).await?;
        (d.generation > generation).then_some(d)
    })
    .await;
    assert_eq!(doc.id, intro.id);
    let now_rows = h.state.store.passages_of(&intro.id).await.unwrap();
    assert!(
        now_rows.iter().any(|r| r.id == kept_row.id),
        "the unchanged passage kept its id"
    );
    assert!(now_rows.iter().all(|r| r.id != changed_row.id));
    assert!(now_rows.iter().any(|r| r.text.contains("Stomata")));
    let on_disk =
        std::fs::read_to_string(h.data_dir.join("sources").join(&intro.id).join("file")).unwrap();
    assert_eq!(on_disk, intro2);
    // The figure can't be found any more.
    let resolved =
        margin_service::retrieval::resolve(&h.state.store, &Scope::Workspace { id: ws.id.clone() })
            .await
            .unwrap();
    assert!(!resolved.source_ids.contains(&fig.id));
    assert_eq!(files(&h, &t, &ws.id).await.len(), 3);

    // The same manifest again: nothing to send.
    let plan = sync(&h, &t, &ws.id, manifest).await;
    assert_eq!(
        plan,
        SyncPlan {
            upload: vec![],
            kept: 3,
            removed: 0,
        }
    );
}

// ---------------------------------------------------------------------------
// 4. Upload rules
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn folder_uploads_are_checked() {
    let h = start().await;
    let t = h.sign_in("MacBook Pro").await;
    let other = h.sign_in("iPhone").await;
    let ws = new_folder(&h, &t, "notes").await;

    // Only the holder sends its files or its manifest.
    let r = upload_file(&h, &other, &ws.id, "a.md", b"hi", false).await;
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (403, "not_yours"));
    assert!(err.message.contains("MacBook Pro"), "{}", err.message);
    let r = sync_raw(&h, &other, &ws.id, vec![listed("a.md", b"hi")]).await;
    assert_eq!(api_error(r).await.1.code, "not_yours");
    let r = sync_raw(&h, &t, "nope", vec![]).await;
    assert_eq!(r.status(), 404);
    let r = sync_raw(
        &h,
        &t,
        &ws.id,
        vec![ManifestFile {
            path: "../x.md".into(),
            ..listed("x.md", b"x")
        }],
    )
    .await;
    assert_eq!(api_error(r).await.1.code, "bad_path");
    // A type folders don't take is ignored by the manifest.
    let plan = sync(
        &h,
        &t,
        &ws.id,
        vec![
            ManifestFile {
                path: "main.rs".into(),
                ..listed("x.md", b"x")
            },
            listed("a.md", b"hi"),
        ],
    )
    .await;
    assert_eq!(plan.upload, ["a.md"]);

    // Not under a topic.
    let topic: margin_shared::Topic = h
        .post(&t, "/api/topics")
        .json(&json!({"title": "T"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let r = h
        .post(&t, routes::SOURCES)
        .query(&[
            ("name", "a.md"),
            ("workspace_id", &ws.id),
            ("topic_id", &topic.id),
        ])
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(api_error(r).await.0, 400);
    let r = upload_file(&h, &t, &ws.id, "main.rs", b"fn main() {}", false).await;
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (400, "unsupported_source"));
    let r = upload_file(&h, &t, &ws.id, "../x.md", b"x", false).await;
    assert_eq!(api_error(r).await.1.code, "bad_path");
    let r = upload_file(&h, &t, "nope", "a.md", b"x", false).await;
    assert_eq!(r.status(), 404);

    // The folder query value is ignored: the folder's name is used.
    let r = h
        .post(&t, routes::SOURCES)
        .query(&[
            ("name", "sub/a.md"),
            ("workspace_id", &ws.id),
            ("folder", "other"),
        ])
        .body("hi")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let s: Source = r.json().await.unwrap();
    assert_eq!(
        (s.folder.as_deref(), s.name.as_str()),
        (Some("notes"), "sub/a.md")
    );

    // No size limit for a folder's files (in parts), but the topic limit stands.
    let big = {
        let mut b = UNREADABLE.as_bytes().to_vec();
        b.resize(60 * 1024 * 1024, b' ');
        b
    };
    let r = upload_file(&h, &t, &ws.id, "huge.pdf", &big, true).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let huge: Source = r.json().await.unwrap();
    assert_eq!(huge.bytes, big.len() as u64);
    let r = h
        .post(&t, routes::SOURCE_UPLOADS)
        .query(&[
            ("name", "huge.pdf"),
            ("topic_id", topic.id.as_str()),
            ("bytes", &big.len().to_string()),
        ])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 413);
    // One request carries at most a part.
    let r = h
        .post(&t, routes::SOURCES)
        .query(&[("name", "big.pdf"), ("workspace_id", &ws.id)])
        .body(vec![b' '; UPLOAD_PART_BYTES as usize + 1])
        .send()
        .await
        .unwrap();
    assert_eq!(api_error(r).await.0, 413);
}

// ---------------------------------------------------------------------------
// 5. Deletion and scoping
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn folder_files_stay_out_of_topics_and_chats_and_go_with_their_folder() {
    let (h, t, ws) = synced_notes().await;
    let all = files(&h, &t, &ws.id).await;
    let ids: HashSet<String> = all.iter().map(|s| s.id.clone()).collect();

    // Not a chat's file, not listed or filed by the agent.
    let r = h
        .post(&t, "/api/chats")
        .json(&json!({"workspace_ids": [ws.id]}))
        .send()
        .await
        .unwrap();
    let chat: ChatDetail = r.json().await.unwrap();
    let r = h
        .patch(&t, &format!("/api/chats/{}", chat.chat.id))
        .json(&json!({"source_ids": [all[0].id]}))
        .send()
        .await
        .unwrap();
    let (status, err) = api_error(r).await;
    assert_eq!((status, err.code.as_str()), (400, "unknown_source"));
    assert_eq!(
        err.message,
        "That file belongs to a folder; attach the folder instead."
    );
    let ctx = tool_ctx(&chat.chat.id, vec![ws.clone()]);
    let out = run_tool(&h, &ctx, "list_sources", json!({"topic_id": null})).await;
    assert!(out.ok);
    let listed: Value = serde_json::from_str(&out.output).unwrap();
    assert!(listed["sources"].as_array().unwrap().is_empty(), "{listed}");
    let topic: margin_shared::Topic = h
        .post(&t, "/api/topics")
        .json(&json!({"title": "T"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let out = run_tool(
        &h,
        &ctx,
        "attach_sources",
        json!({"source_ids": [all[0].id], "topic_id": topic.id}),
    )
    .await;
    assert!(!out.ok, "{}", out.output);
    assert!(
        files(&h, &t, &ws.id)
            .await
            .iter()
            .all(|s| s.topic_id.is_none())
    );

    // A sub-folder listing, a bad path and a bad pattern.
    let out = run_tool(
        &h,
        &ctx,
        "list_files",
        json!({"workspace_id": ws.id, "path": "../x"}),
    )
    .await;
    assert!(
        out.output.contains("isn't a path inside the folder"),
        "{}",
        out.output
    );
    let out = run_tool(
        &h,
        &ctx,
        "find_files",
        json!({"workspace_id": ws.id, "path": null, "pattern": "["}),
    )
    .await;
    assert!(
        out.output.contains("isn't a valid pattern"),
        "{}",
        out.output
    );
    let none = tool_ctx(&chat.chat.id, vec![]);
    let out = run_tool(
        &h,
        &none,
        "list_files",
        json!({"workspace_id": ws.id, "path": null}),
    )
    .await;
    assert!(
        out.output.contains(
            "No folder is attached to this chat. The owner can attach one with Attach folder."
        ),
        "{}",
        out.output
    );

    // The purge leaves a two-day-old folder file; deleting the chat leaves them.
    let mut old = all[0].clone();
    old.created_at = Utc::now() - chrono::Duration::days(2);
    h.state.store.put(db::SOURCE, &old.id, &old).await.unwrap();
    margin_service::server::recover(&h.state).await.unwrap();
    assert!(
        h.state
            .store
            .get::<Source>(db::SOURCE, &old.id)
            .await
            .unwrap()
            .is_some()
    );
    let r = h
        .delete(&t, &format!("/api/chats/{}", chat.chat.id))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success());
    assert_eq!(files(&h, &t, &ws.id).await.len(), 3);

    // Removing the folder deletes its files: a SourceDeleted each, then WorkspaceDeleted.
    let mut socket = h.socket(&t).await;
    let r = h
        .delete(&t, &format!("/api/workspaces/{}", ws.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let events = socket
        .until(|e| matches!(e, Event::WorkspaceDeleted { .. }))
        .await;
    let deleted: HashSet<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::SourceDeleted { id } => Some(id.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(deleted, ids);
    for id in &ids {
        assert!(document(&h, id).await.is_none());
        assert!(h.state.store.passages_of(id).await.unwrap().is_empty());
        assert!(!h.data_dir.join("sources").join(id).exists());
    }
    assert!(rows(&h, &ws.id).await.is_empty());
    assert_eq!(
        h.get(&t, &routes::workspace_files(&ws.id))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn revoking_the_holding_device_deletes_its_folders_files() {
    let h = start().await;
    let mac = h.sign_in("MacBook Pro").await;
    let phone = h.sign_in("iPhone").await;
    let ws = new_folder(&h, &mac, "notes").await;
    sync(&h, &mac, &ws.id, vec![listed("a.md", b"Roots grow down.")]).await;
    let s = upload_ok(&h, &mac, &ws.id, "a.md", b"Roots grow down.").await;
    folder_read(&h, &mac, &ws.id, 1).await;
    let me: Value = h
        .get(&mac, routes::ME)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let device_id = me["device"]["id"]
        .as_str()
        .or_else(|| me["id"].as_str())
        .unwrap()
        .to_string();
    let r = h
        .delete(&phone, &format!("/api/devices/{device_id}"))
        .send()
        .await
        .unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    assert!(
        h.state
            .store
            .get::<Source>(db::SOURCE, &s.id)
            .await
            .unwrap()
            .is_none()
    );
    assert!(document(&h, &s.id).await.is_none());
    assert!(rows(&h, &ws.id).await.is_empty());
}

// ---------------------------------------------------------------------------
// 6. Queue priority
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_chat_file_is_read_before_queued_folder_files() {
    let h = start_with(Setup {
        model_downloaded: false,
        ..Setup::default()
    })
    .await;
    let t = h.sign_in("MacBook Pro").await;
    let ws = new_folder(&h, &t, "notes").await;
    let names: Vec<String> = (1..=5).map(|i| format!("n{i}.md")).collect();
    sync(
        &h,
        &t,
        &ws.id,
        names
            .iter()
            .map(|n| listed(n, format!("Note {n} body.").as_bytes()))
            .collect(),
    )
    .await;
    for n in &names {
        upload_ok(&h, &t, &ws.id, n, format!("Note {n} body.").as_bytes()).await;
    }
    // Uploaded after them, before the model is there.
    let r = h.upload(&t, "chat.md", None, "A chat file.").await;
    assert_eq!(r.status(), 200);
    let chat_file: Source = r.json().await.unwrap();
    let mut socket = h.socket(&t).await;
    let r = h.post(&t, "/api/embedding/download").send().await.unwrap();
    assert_eq!(r.status(), 200);
    let mut ready: Vec<String> = Vec::new();
    while ready.len() < 6 {
        let Some(e) = socket.next_event().await else {
            panic!("socket closed")
        };
        if let Event::SourceChanged { source } = e
            && matches!(source.status, SourceStatus::Ready { .. })
            && !ready.contains(&source.id)
        {
            ready.push(source.id);
        }
    }
    let chat_at = ready.iter().position(|id| *id == chat_file.id).unwrap();
    assert!(
        chat_at <= 1,
        "the chat file was read {chat_at}th: {ready:?}"
    );
}

// ---------------------------------------------------------------------------
// 7. Scope
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_folders_passages_are_found_through_that_folder_only() {
    let (h, t, a) = synced_notes().await;
    let b = new_folder(&h, &t, "other").await;
    sync(
        &h,
        &t,
        &b.id,
        vec![listed("b.md", b"Glaciers move slowly.")],
    )
    .await;
    let other = upload_ok(&h, &t, &b.id, "b.md", b"Glaciers move slowly.").await;
    folder_read(&h, &t, &b.id, 1).await;
    let a_files: HashSet<String> = files(&h, &t, &a.id)
        .await
        .into_iter()
        .map(|s| s.id)
        .collect();
    let in_a =
        margin_service::retrieval::resolve(&h.state.store, &Scope::Workspace { id: a.id.clone() })
            .await
            .unwrap();
    let in_b =
        margin_service::retrieval::resolve(&h.state.store, &Scope::Workspace { id: b.id.clone() })
            .await
            .unwrap();
    for id in &a_files {
        assert!(in_a.source_ids.contains(id));
        assert!(!in_b.source_ids.contains(id));
    }
    assert!(in_b.source_ids.contains(&other.id));
    assert!(!in_a.source_ids.contains(&other.id));
    // And through the context API, the same.
    let hits: Vec<margin_shared::context::Hit> = h
        .post(&t, routes::CONTEXT_SEARCH)
        .json(
            &json!({"text": "glaciers move", "scope": {"scope": "workspace", "id": a.id}, "k": 8}),
        )
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        hits.iter().all(|hit| a_files.contains(&hit.document_id)),
        "{hits:?}"
    );
    let _: Option<Chat> = None;
}

// ---------------------------------------------------------------------------
// Needing 4-agent's margin_agent (`sync_folder`, `ToolOutput.images`).
// ---------------------------------------------------------------------------

/// A chat with `ws` attached whose model makes `calls` then answers; waits for it.
async fn folder_turn(h: &Harness, t: &str, ws: &str, calls: &[(&str, &str, &str)]) -> ChatDetail {
    h.generator
        .agent
        .push(Scripted::Events(tool_calls_reply(calls)));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Done."])));
    let r = h
        .post(t, "/api/chats")
        .json(&json!({"workspace_ids": [ws], "message": {"text": "Look at my folder"}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let chat: ChatDetail = r.json().await.unwrap();
    wait_idle(h, t, &chat.chat.id, 3).await
}

/// D13 test 3: `sync_folder` goes to the holding device; offline and unattached folders
/// get their texts.
#[tokio::test(flavor = "multi_thread")]
async fn sync_folder_asks_the_holding_device() {
    let h = start().await;
    let t = h.sign_in("MacBook Pro").await;
    h.configure_ai(&t).await;
    let ws = new_folder(&h, &t, "notes").await;
    let mut device = h.socket(&t).await;
    let args = json!({"workspace_id": ws.id}).to_string();
    let calls = [("call_sync", "sync_folder", args.as_str())];
    let run = folder_turn(&h, &t, &ws.id, &calls);
    let answer = async {
        let request = match device.next_directed().await {
            margin_shared::DirectedFrame::ToolRequest { request } => request,
            other => panic!("expected a tool request, got {other:?}"),
        };
        assert_eq!(
            request.call,
            margin_shared::DeviceToolCall::SyncFolder {
                workspace_id: ws.id.clone()
            }
        );
        device
            .send_frame(&margin_shared::DeviceFrame::ToolResult {
                request_id: request.request_id,
                result: margin_shared::DeviceToolResult::Ok {
                    output: margin_shared::DeviceToolOutput::Done {
                        message: "Synced “notes”: 3 files to send, 1 removed; sending now.".into(),
                    },
                },
            })
            .await;
    };
    let (d, ()) = tokio::join!(run, answer);
    assert_eq!(
        output_sent(&h, "call_sync"),
        Value::String("Synced “notes”: 3 files to send, 1 removed; sending now.".into())
    );
    assert!(d.items.iter().any(|i| matches!(&i.kind, ChatItemKind::ToolResult { call_id, ok: true, .. } if call_id == "call_sync")));
    let offered_tools = offered(&h.generator.agent.requests()[0]);
    for name in ["list_files", "find_files", "sync_folder"] {
        assert!(
            offered_tools.iter().any(|n| n == name),
            "{name}: {offered_tools:?}"
        );
    }

    // With the device gone: the offline text.
    device.close().await;
    folder_turn(&h, &t, &ws.id, &[("call_off", "sync_folder", &args)]).await;
    let out = output_sent(&h, "call_off").to_string();
    assert!(
        out.contains("MacBook Pro isn't connected to Margin right now"),
        "{out}"
    );
    // A folder that isn't attached: the check text.
    folder_turn(
        &h,
        &t,
        &ws.id,
        &[(
            "call_bad",
            "sync_folder",
            &json!({"workspace_id": "nope"}).to_string(),
        )],
    )
    .await;
    let out = output_sent(&h, "call_bad").to_string();
    assert!(
        out.contains("No folder attached to this chat has the workspace_id “nope”"),
        "{out}"
    );
}

/// D13 test 1, the pixels: `view_picture` stores a user message with the picture right
/// after its `function_call_output`, and the next request carries it.
#[tokio::test(flavor = "multi_thread")]
async fn view_picture_sends_the_picture_to_the_model() {
    let (h, t, ws) = synced_notes().await;
    let pdf = by_name(&files(&h, &t, &ws.id).await, "paper.pdf").clone();
    let d = folder_turn(
        &h,
        &t,
        &ws.id,
        &[(
            "call_view",
            "view_picture",
            &json!({"document_id": pdf.id, "page": 3}).to_string(),
        )],
    )
    .await;
    let result = d
        .items
        .iter()
        .find(|i| matches!(&i.kind, ChatItemKind::ToolResult { call_id, .. } if call_id == "call_view"))
        .unwrap();
    let stored: margin_service::chats::StoredChatItem = h
        .state
        .store
        .get(db::CHAT_ITEM, &result.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(stored.model_items.len(), 2, "{:?}", stored.model_items);
    assert_eq!(stored.model_items[0]["type"], "function_call_output");
    let picture = &stored.model_items[1];
    assert_eq!(picture["role"], "user");
    let images: Vec<&Value> = picture["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|c| c["type"] == "input_image")
        .collect();
    assert_eq!(images.len(), 1);
    assert!(
        images[0]["image_url"]
            .as_str()
            .unwrap()
            .starts_with("data:image/jpeg;base64,")
    );
    // The answer step was sent the picture.
    let requests = h.generator.agent.requests();
    let last = requests.last().unwrap();
    assert!(
        last.input.iter().any(|i| i["role"] == "user"
            && i["content"]
                .as_array()
                .is_some_and(|c| c.iter().any(|p| p["type"] == "input_image"))),
        "no picture in the next request"
    );
}

/// The manifest route takes up to `MANIFEST_MAX_BYTES` (64 MB; the 2 MB of every other
/// JSON route would cap a folder at about 10 000 files): at the limit the body is read
/// (junk is then a 400), one byte more is a 413 (phase 5, D7).
#[tokio::test(flavor = "multi_thread")]
async fn the_manifest_body_stops_at_its_limit() {
    use margin_service::folders::MANIFEST_MAX_BYTES;
    let h = start().await;
    let t = h.sign_in("Mac").await;
    let send = |n: usize| {
        h.post(&t, "/api/workspaces/no-such-folder/sync")
            .header("content-type", "application/json")
            .body(vec![b'x'; n])
            .send()
    };
    assert_eq!(send(MANIFEST_MAX_BYTES).await.unwrap().status(), 400);
    assert_eq!(send(MANIFEST_MAX_BYTES + 1).await.unwrap().status(), 413);
    const { assert!(MANIFEST_MAX_BYTES > routes::JSON_BODY_MAX_BYTES) };
}
