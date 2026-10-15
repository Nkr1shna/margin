//! R1 end to end (`docs/agent-rag-architecture.md` §6.4, tests 1–6): a course on the
//! fixture corpus, the ingest diff, the context API, a turn's traces and ⌘K, through
//! the real router with the harness's fakes (test 5, the passage migration, went with
//! the migration).
//!
//! The corpus documents are read with `FakeSourceTools` and embedded with the harness's
//! `FakeEmbedder` (bag of words, deterministic). §6.4 asks for `RecordedEmbedder`, but no
//! embeddings are recorded yet (the owner runs `margin-eval record-embeddings` on the
//! Mac); swap it in once `evals/` holds them.

mod common;

use std::collections::{HashMap, HashSet};

use chrono::Utc;
use common::*;
use margin_agent::scripted::{text_reply, tool_calls_reply};
use margin_agent::{NoDevices, RetryPolicy, Scripted, ToolContext};
use margin_service::ask::{CoursePlan, PlannedLesson, StoredProposal};
use margin_service::db::{self, PassageRow};
use margin_shared::context::{
    ContextChanges, Document, DocumentOutline, DocumentStatus, Hit, LibraryKind, Origin, Scope,
    TraceKind, TraceRollup, TraceRow,
};
use margin_shared::pipeline::Route;
use margin_shared::{
    ApiError, Card, ChatDetail, ChatItemKind, ChatState, DeviceRef, Event, Lesson, LessonStatus,
    Library, Note, Proposal, SearchHit, SearchKind, Source, SourceStatus, Topic,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Prices for the harness's model, so model calls are priced at write time.
const TEST_PRICES: &str = "[models.test-model]\ninput = 1.0\ncached_input = 0.5\noutput = 2.0\n";

/// Signed in, AI configured, the test model priced.
async fn signed_in() -> (Harness, String) {
    let h = start_with(Setup {
        files: vec![("prices.toml".into(), TEST_PRICES.as_bytes().to_vec())],
        ..Setup::default()
    })
    .await;
    *h.state.turns.retry.lock().unwrap() = RetryPolicy::immediate();
    let t = h.sign_in("Mac").await;
    h.configure_ai(&t).await;
    (h, t)
}

/// A fixture of `evals/corpus`.
fn corpus(path: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../evals/corpus")
        .join(path);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
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

async fn new_topic(h: &Harness, t: &str, parent: Option<&str>, title: &str) -> Topic {
    let r = h
        .post(t, "/api/topics")
        .json(&json!({"parent_id": parent, "title": title}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// Uploads a corpus file through `POST /api/sources`.
async fn upload_corpus(h: &Harness, t: &str, path: &str, topic: Option<&str>) -> Source {
    let name = path.rsplit('/').next().unwrap();
    let r = h.upload(t, name, topic, corpus(path)).await;
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// Waits until every source in `ids` is Ready in the library.
async fn wait_ready(h: &Harness, t: &str, ids: &[&str]) {
    eventually(|| async {
        let lib = library(h, t).await;
        ids.iter()
            .all(|id| {
                lib.sources
                    .iter()
                    .any(|s| s.id == *id && matches!(s.status, SourceStatus::Ready { .. }))
            })
            .then_some(())
    })
    .await;
}

/// `GET /api/traces` with `query`: the rows and the raw body.
async fn traces(h: &Harness, t: &str, query: &[(&str, String)]) -> (Vec<TraceRow>, String) {
    let r = h.get(t, "/api/traces").query(query).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let body = r.text().await.unwrap();
    let rows = body
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    (rows, body)
}

async fn traces_of(h: &Harness, t: &str, trace_id: &str) -> Vec<TraceRow> {
    traces(h, t, &[("trace_id", trace_id.to_string())]).await.0
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

/// Starts a chat with `text` and these sources attached, and waits for the turn.
async fn chat_turn(h: &Harness, t: &str, text: &str, source_ids: &[&str]) -> ChatDetail {
    let r = h
        .post(t, "/api/chats")
        .json(&json!({"message": {"text": text, "source_ids": source_ids}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    let chat: ChatDetail = r.json().await.unwrap();
    eventually(|| async {
        let d = chat_detail(h, t, &chat.chat.id).await;
        (d.chat.state == ChatState::Idle
            && d.chat.title_by != margin_shared::TitleSource::FirstLine)
            .then_some(d)
    })
    .await
}

/// The search tool's arguments with every field given (the schema is strict).
fn search_args(query: &str) -> String {
    json!({"query": query, "scope": null, "k": null, "modalities": null}).to_string()
}

/// The full output the model got for `call_id`, from the requests it was sent.
fn tool_output_sent(h: &Harness, call_id: &str) -> String {
    h.generator
        .agent
        .requests()
        .iter()
        .flat_map(|r| r.input.iter())
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == call_id)
        .and_then(|i| i["output"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no output for {call_id}"))
}

fn tool_ctx(chat_id: &str) -> ToolContext {
    ToolContext {
        chat_id: chat_id.into(),
        turn_id: "turn".into(),
        call_id: "call".into(),
        origin_device: DeviceRef {
            device_id: "d".into(),
            name: "Mac".into(),
        },
        workspaces: Vec::new(),
        devices: std::sync::Arc::new(NoDevices),
    }
}

async fn document(h: &Harness, id: &str) -> Document {
    margin_service::ingest::document(&h.state, id)
        .await
        .unwrap()
        .expect("the document exists")
}

/// Waits until every document is Ready and the index generation held still between
/// two looks (no re-index in flight), so a repeated retrieval has the same cache key.
async fn index_settled(h: &Harness) {
    let last = std::sync::Mutex::new(None::<u64>);
    eventually(|| async {
        let changes = margin_service::ingest::changes_since(&h.state, 0)
            .await
            .unwrap();
        let ready = changes
            .changed
            .iter()
            .all(|d| matches!(d.status, DocumentStatus::Ready { .. }));
        let mut last = last.lock().unwrap();
        let same = *last == Some(changes.generation);
        *last = Some(changes.generation);
        (ready && same).then_some(())
    })
    .await;
}

// ---------------------------------------------------------------------------
// 1. A course on the fixture corpus
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_course_on_the_corpus_is_retrieved_planned_written_cited_and_cached() {
    let (h, t) = signed_in().await;
    let topic = new_topic(&h, &t, None, "Corpus").await;
    let other = new_topic(&h, &t, None, "Elsewhere").await;
    // Four documents in the topic, two attached to the chat only, one in another topic
    // that the chat must not reach.
    let mut in_topic = Vec::new();
    for f in [
        "bayes.md",
        "fourier-series.md",
        "photosynthesis.md",
        "tcp-congestion.md",
    ] {
        in_topic.push(upload_corpus(&h, &t, f, Some(&topic.id)).await);
    }
    let lens = upload_corpus(&h, &t, "sources/lens-optics.txt", None).await;
    let rc = upload_corpus(&h, &t, "sources/rc-circuits.txt", None).await;
    let outsider = upload_corpus(&h, &t, "sources/glacier-scan.txt", Some(&other.id)).await;
    let docs: Vec<&Source> = in_topic.iter().chain([&lens, &rc]).collect();
    let ids: Vec<&str> = docs.iter().map(|s| s.id.as_str()).collect();
    wait_ready(&h, &t, &ids).await;
    wait_ready(&h, &t, &[&outsider.id]).await;

    // Passages carry their locators, hashes and scopes.
    let mut rows_by_id: HashMap<String, PassageRow> = HashMap::new();
    for s in &docs {
        let rows = h.state.store.passages_of(&s.id).await.unwrap();
        assert!(rows.len() >= 3, "{}: {} passages", s.name, rows.len());
        for r in &rows {
            assert_eq!(r.document_id, s.id);
            assert_eq!(r.chunk_hash.len(), 64);
            let (Some(a), Some(b)) = (r.start_line, r.end_line) else {
                panic!("{}: no lines on {r:?}", s.name)
            };
            assert!(a >= 1 && a <= b);
            if s.topic_id.is_some() {
                assert_eq!(r.scope_ids, std::slice::from_ref(&topic.id));
            }
        }
        if s.name.ends_with(".md") {
            // The fake chunker names sections by `# ` lines: each file's title.
            let title = corpus(&s.name).lines().next().unwrap()[2..].to_string();
            assert!(rows.iter().all(|r| r.section_path == title), "{}", s.name);
        }
        rows_by_id.extend(rows.into_iter().map(|r| (r.id.clone(), r)));
    }

    // The chat: the model searches with the default scope (this chat's files, then its
    // topic), then proposes a course with a survey.
    let bayes = &in_topic[0];
    let source_ids: Vec<String> = ids.iter().map(|s| s.to_string()).collect();
    let draft = json!({
        "prompt": "A course from my notes",
        "topic_title": "Draft course",
        "subject_title": "Science",
        "lessons": [
            {"title": "Draft one", "summary": "First", "minutes": 5},
            {"title": "Draft two", "summary": "Second", "minutes": 5},
        ],
        "level": "intermediate",
        "with_videos": false,
        "with_quiz": false,
        "parent_topic_id": topic.id,
        "source_ids": source_ids,
        "survey": true,
    });
    let planned = |title: &str, queries: [&str; 2]| PlannedLesson {
        title: title.into(),
        summary: format!("{title}, from the notes"),
        minutes: 10,
        queries: queries.iter().map(|q| q.to_string()).collect(),
        must_cite: vec![],
    };
    let plan = CoursePlan {
        topic_title: "Planned course".into(),
        subject_title: "Science".into(),
        lessons: vec![
            planned(
                "Bayes and base rates",
                [
                    "prior posterior likelihood evidence",
                    "base rate fallacy false positives",
                ],
            ),
            planned(
                "Capacitors charging",
                [
                    "capacitor resistor time constant",
                    "charging current battery voltage",
                ],
            ),
        ],
    };
    *h.generator.plan_reply.lock().unwrap() = Some(plan.clone());
    let agent = &h.generator.agent;
    agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_search",
        "search",
        &search_args("prior posterior likelihood glacier firn ablation"),
    )])));
    agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_propose",
        "propose_course",
        &draft.to_string(),
    )])));
    agent.push(Scripted::Events(text_reply(&["Here is a plan."])));
    h.title_reply("Corpus course");
    let d = chat_turn(
        &h,
        &t,
        "A course from my notes, please",
        &[&bayes.id, &lens.id, &rc.id],
    )
    .await;
    let chat_id = d.chat.id.clone();
    let turn_id = d.items[0].turn_id.clone();

    // Attached files are searchable from the chat.
    for s in [&lens, &rc, bayes] {
        let rows = h.state.store.passages_of(&s.id).await.unwrap();
        assert!(
            rows.iter().all(|r| r.scope_ids.contains(&chat_id)),
            "{}",
            s.name
        );
    }

    // The model was offered the retrieval family, not the old tool.
    let requests = agent.requests();
    let tools: Vec<&str> = requests[0]
        .tools
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for name in [
        "search",
        "read_passage",
        "document_outline",
        "view_picture",
        "propose_course",
    ] {
        assert!(tools.contains(&name), "{name} missing from {tools:?}");
    }
    assert!(!tools.contains(&"search_sources"));

    // search: under 16 KB, numbered, inside the default scope, with a Passages card.
    let output = tool_output_sent(&h, "call_search");
    assert!(output.len() <= margin_service::agent_tools::RETRIEVAL_OUTPUT_MAX_BYTES);
    let v: Value = serde_json::from_str(&output).unwrap();
    let passages = v["passages"].as_array().unwrap();
    assert!(!passages.is_empty());
    for (i, p) in passages.iter().enumerate() {
        assert_eq!(p["n"], i as u64 + 1);
        let doc = p["document_id"].as_str().unwrap();
        assert!(ids.contains(&doc), "{doc} is outside the chat's scope");
        assert!(p["start_line"].is_u64() && p["end_line"].is_u64());
    }
    assert!(
        passages
            .iter()
            .any(|p| p["document_id"] == bayes.id.as_str())
    );
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
        .expect("a Passages card");
    assert_eq!(card.passages.len(), passages.len());
    assert!(
        card.passages
            .iter()
            .all(|c| c.document_id.is_some() && c.start_line.is_some())
    );

    // The planner saw the survey and every document's contents.
    let (plan_req, plan_opts) = h.generator.plan_requests.lock().unwrap()[0].clone();
    assert_eq!(plan_opts.route, Route::Write);
    assert_eq!(plan_req.lesson_count, 2);
    assert_eq!(plan_req.contents.len(), docs.len());
    let names: HashSet<&str> = docs.iter().map(|s| s.name.as_str()).collect();
    assert!(
        plan_req
            .contents
            .iter()
            .all(|c| names.contains(c.document.as_str()))
    );
    let bayes_contents = plan_req
        .contents
        .iter()
        .find(|c| c.document == "bayes.md")
        .unwrap();
    assert_eq!(bayes_contents.sections, ["Bayesian reasoning"]);
    assert!(!plan_req.survey.is_empty());
    assert!(
        plan_req
            .survey
            .iter()
            .all(|p| names.contains(p.source.as_str()))
    );
    // The survey's passages are the ones its retrieval (traced under the turn) packed.
    let turn_rows = traces_of(&h, &t, &turn_id).await;
    let survey_row = turn_rows
        .iter()
        .filter(|r| r.kind == TraceKind::Retrieval)
        .find(|r| {
            matches!(&r.scope, Some(Scope::Union { scopes })
                if scopes.iter().any(|s| matches!(s, Scope::Sources { .. })))
        })
        .expect("a survey retrieval");
    assert!(survey_row.packed_ids.len() >= plan_req.survey.len());
    for id in &survey_row.packed_ids {
        let text = rows_by_id[id].text.trim();
        assert!(
            plan_req.survey.iter().any(|p| p.text.contains(text)),
            "survey misses {id}"
        );
    }
    let planner = turn_rows
        .iter()
        .find(|r| r.kind == TraceKind::ModelCall && r.prompt_version.as_deref() == Some("plan-v1"))
        .expect("the planner call is traced");
    assert_eq!(planner.route, Some(Route::Write));
    assert!(planner.usage.is_some() && planner.cost_usd.is_some());

    // The plan's questions are kept on the proposal.
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
        .expect("an outline card");
    let stored: StoredProposal = h
        .state
        .store
        .get(db::PROPOSAL, &proposal_id)
        .await
        .unwrap()
        .unwrap();
    let kept = stored.plan.expect("the plan is stored");
    for (got, want) in kept.lessons.iter().zip(&plan.lessons) {
        assert_eq!(got.title, want.title);
        assert_eq!(got.queries, want.queries);
    }
    let proposal: Proposal = h
        .get(&t, &format!("/api/proposals/{proposal_id}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let outline = proposal.outline.clone().unwrap();
    assert_eq!(outline.lessons[0].title, "Bayes and base rates");

    // A small writing window: the budget (half of it), not a fixed k, bounds a lesson's
    // passages.
    const WINDOW: u64 = 1_000;
    let r = h
        .patch(&t, "/api/settings")
        .json(&json!({"pipeline": {"packer": {"name": "mmr-v1", "params": {"window_tokens": WINDOW}}}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    *h.generator.lesson_body.lock().unwrap() = Some("From the notes.[^1]".into());
    let r = h
        .post(&t, &format!("/api/proposals/{proposal_id}/accept"))
        .json(&json!({"outline": outline}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let course: Topic = r.json().await.unwrap();
    let lessons: Vec<Lesson> = eventually(|| async {
        let ls: Vec<Lesson> = library(&h, &t)
            .await
            .lessons
            .into_iter()
            .filter(|l| l.topic_id == course.id)
            .collect();
        (ls.len() == 2 && ls.iter().all(|l| l.status == LessonStatus::Ready)).then_some(ls)
    })
    .await;

    let requests = h.generator.lesson_requests.lock().unwrap().clone();
    assert_eq!(requests.len(), 2);
    let mut first_packed: HashMap<String, Vec<String>> = HashMap::new();
    for lesson in &lessons {
        let ctx = requests
            .iter()
            .find(|c| c.outline[c.index].title == lesson.title)
            .unwrap();
        let rows = traces_of(&h, &t, &lesson.id).await;
        let retrieval: Vec<&TraceRow> = rows
            .iter()
            .filter(|r| r.kind == TraceKind::Retrieval)
            .collect();
        assert_eq!(retrieval.len(), 1, "{}", lesson.title);
        let packed = &retrieval[0].packed_ids;
        assert!(!packed.is_empty() && !ctx.passages.is_empty());
        assert!(retrieval[0].packed_tokens.unwrap() <= WINDOW / 2);
        assert!(ctx.passages.len() <= packed.len());
        // The writer got exactly the passages the trace says were packed.
        for id in packed {
            let text = rows_by_id[id].text.trim();
            assert!(ctx.passages.iter().any(|p| p.text.contains(text)), "{id}");
        }
        for p in &ctx.passages {
            assert!(
                packed
                    .iter()
                    .any(|id| p.text.contains(rows_by_id[id].text.trim()))
            );
        }
        first_packed.insert(lesson.id.clone(), packed.clone());
        // Citations resolve to a document and its lines.
        assert_eq!(lesson.citations.len(), 1);
        let c = &lesson.citations[0];
        let doc = c.document_id.as_deref().expect("a document id");
        assert!(ids.contains(&doc));
        assert!(rows_by_id.values().any(|r| r.document_id == doc
            && r.start_line == c.start_line.map(i64::from)
            && c.end_line.is_some()));
        // The lesson's call is traced with its prompt version, usage and cost.
        let call = rows
            .iter()
            .find(|r| r.kind == TraceKind::ModelCall)
            .expect("the lesson call is traced");
        assert_eq!(call.prompt_version.as_deref(), Some("lesson-v2"));
        assert_eq!(call.job_id.as_deref(), Some(lesson.id.as_str()));
        assert_eq!(call.usage.unwrap().input, fake_usage().input_tokens);
        assert!(call.cost_usd.is_some());
    }
    // The Bayes lesson's questions found the Bayes notes.
    let bayes_lesson = lessons
        .iter()
        .find(|l| l.title == "Bayes and base rates")
        .unwrap();
    assert!(
        first_packed[&bayes_lesson.id]
            .iter()
            .any(|id| rows_by_id[id].document_id == bayes.id)
    );

    // Regenerating with the index unchanged reads the retrieval cache. (Writing the
    // lessons re-indexed them as library documents, which moved the index generation,
    // so the first regenerate after that refills the cache.)
    let regenerate = || async {
        index_settled(&h).await;
        let n = h.generator.lesson_requests.lock().unwrap().len();
        eventually(|| async {
            let r = h
                .post(&t, &format!("/api/lessons/{}/regenerate", bayes_lesson.id))
                .send()
                .await
                .unwrap();
            (r.status() == 200).then_some(())
        })
        .await;
        eventually(|| async {
            let ls = library(&h, &t).await.lessons;
            let l = ls.iter().find(|l| l.id == bayes_lesson.id).unwrap();
            (l.status == LessonStatus::Ready
                && h.generator.lesson_requests.lock().unwrap().len() > n)
                .then_some(())
        })
        .await;
    };
    regenerate().await;
    regenerate().await;
    let rows = traces_of(&h, &t, &bayes_lesson.id).await;
    let retrievals: Vec<&TraceRow> = rows
        .iter()
        .filter(|r| r.kind == TraceKind::Retrieval)
        .collect();
    assert_eq!(retrievals.len(), 3);
    let last = retrievals[2];
    assert_eq!(last.cache_hit, Some(true), "{retrievals:#?}");
    assert_eq!(last.packed_ids, retrievals[1].packed_ids);
}

// ---------------------------------------------------------------------------
// 2. The ingest diff
// ---------------------------------------------------------------------------

const RUNBOOK: &str = "# Alpha\n\nQuokkas bask on Rottnest island.\n\n\
Wombats dig burrows under eucalyptus.\n\n\
# Beta\n\nNumbats eat termites with long tongues.\n\n\
# Gamma\n\nBilbies forage at night in spinifex.\n\n\
Echidnas lay leathery eggs.";

#[tokio::test(flavor = "multi_thread")]
async fn an_edited_agent_document_reembeds_only_the_changed_section() {
    let (h, t) = signed_in().await;
    let ingest = |text: &str| {
        h.post(&t, "/api/context/ingest")
            .json(&json!({"name": "runbook.md", "origin": {"origin": "agent", "name": "test"}, "text": text}))
            .send()
    };
    let r = ingest(RUNBOOK).await.unwrap();
    assert_eq!(r.status(), 200);
    let doc: Document = r.json().await.unwrap();
    let ready = eventually(|| async {
        let d = document(&h, &doc.id).await;
        (d.status == DocumentStatus::Ready { passages: 5 }).then_some(d)
    })
    .await;
    let before = h.state.store.passages_of(&doc.id).await.unwrap();
    assert_eq!(
        before
            .iter()
            .map(|r| r.section_path.as_str())
            .collect::<Vec<_>>(),
        ["Alpha", "Alpha", "Beta", "Gamma", "Gamma"]
    );

    let mut socket = h.socket(&t).await;
    let edited = RUNBOOK.replace(
        "Numbats eat termites with long tongues.",
        "Numbats eat termites, thousands a day.",
    );
    let r = ingest(&edited).await.unwrap();
    assert_eq!(r.status(), 200);
    let again: Document = r.json().await.unwrap();
    assert_eq!(
        again.id, doc.id,
        "the same agent and name is the same document"
    );
    let events = socket
        .until(|e| matches!(e, Event::DocumentChanged { document } if document.id == doc.id && document.generation > ready.generation))
        .await;
    // A later event marks the end of what the ingest emitted.
    let marker = new_topic(&h, &t, None, "Marker").await;
    let rest = socket
        .until(|e| matches!(e, Event::TopicChanged { topic } if topic.id == marker.id))
        .await;
    let changed = events
        .iter()
        .chain(&rest)
        .filter(|e| matches!(e, Event::DocumentChanged { document } if document.id == doc.id))
        .count();
    assert_eq!(changed, 1);

    let after = document(&h, &doc.id).await;
    assert_eq!(after.status, DocumentStatus::Ready { passages: 5 });
    assert!(after.generation > ready.generation);
    let rows = h.state.store.passages_of(&doc.id).await.unwrap();
    for i in [0, 1, 3, 4] {
        assert_eq!(rows[i].id, before[i].id);
    }
    assert_ne!(rows[2].id, before[2].id);

    let (ingests, _) = traces(
        &h,
        &t,
        &[
            ("kind", "ingest".into()),
            ("trace_id", format!("ingest:{}", doc.id)),
        ],
    )
    .await;
    assert_eq!(ingests.len(), 2);
    let last = &ingests[1];
    assert_eq!(last.document_id.as_deref(), Some(doc.id.as_str()));
    assert_eq!(last.actor.as_deref(), Some("agent:test"));
    assert_eq!(
        (last.chunks_new, last.chunks_kept, last.chunks_deleted),
        (Some(1), Some(4), Some(1))
    );

    let changes: ContextChanges = h
        .get(&t, "/api/context/changes")
        .query(&[("since", ready.generation.to_string())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(changes.changed.iter().any(|d| d.id == doc.id));
    assert!(changes.generation >= after.generation);
}

// ---------------------------------------------------------------------------
// 3. The context API
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_context_api_reads_what_the_agents_tools_read() {
    let (h, t) = signed_in().await;
    let topic = new_topic(&h, &t, None, "Corpus").await;
    let mut ids = Vec::new();
    for f in ["bayes.md", "fourier-series.md", "tcp-congestion.md"] {
        ids.push(upload_corpus(&h, &t, f, Some(&topic.id)).await.id);
    }
    let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
    wait_ready(&h, &t, &refs).await;

    // The same hits as the agent's search tool, for the same scope.
    let query = "congestion window slow start packet loss";
    let r = h
        .post(&t, "/api/context/search")
        .json(&json!({"text": query, "scope": {"scope": "topic", "id": topic.id}, "k": 12}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let hits: Vec<Hit> = r.json().await.unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].document_id, ids[2]);
    let registry = margin_service::agent_tools::registry(&h.state);
    let tool = registry.get("search").unwrap().clone();
    // The tool acts as its chat's owner (L1), so it runs in one of the owner's chats.
    let chat: margin_shared::ChatDetail = h
        .post(&t, "/api/chats")
        .json(&json!({}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let out = tool
        .run(
            &tool_ctx(&chat.chat.id),
            json!({"query": query, "scope": {"topic_id": topic.id, "source_ids": null, "workspace_ids": null, "chat_files": null, "library": null}, "k": 12, "modalities": null}),
        )
        .await;
    assert!(out.ok, "{}", out.output);
    let v: Value = serde_json::from_str(&out.output).unwrap();
    let from_tool: Vec<(String, Option<u64>, Option<u64>, String)> = v["passages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| {
            (
                p["document_id"].as_str().unwrap().to_string(),
                p["start_line"].as_u64(),
                p["end_line"].as_u64(),
                p["section"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    let from_api: Vec<(String, Option<u64>, Option<u64>, String)> = hits
        .iter()
        .map(|h| {
            (
                h.document_id.clone(),
                h.start_line.map(u64::from),
                h.end_line.map(u64::from),
                h.section_path.clone(),
            )
        })
        .collect();
    assert_eq!(from_api, from_tool);

    // Outline and passages of a document.
    let outline: DocumentOutline = h
        .get(&t, &format!("/api/context/documents/{}/outline", ids[0]))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(outline.document_id, ids[0]);
    assert_eq!(outline.sections.len(), 1);
    assert_eq!(outline.sections[0].section_path, "Bayesian reasoning");
    let last = outline.sections[0].to_ord;
    assert!(last >= 2);
    let rows: Vec<Hit> = h
        .get(&t, &format!("/api/context/documents/{}/passages", ids[0]))
        .query(&[("from", "1"), ("to", "2")])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let stored = h.state.store.passages_of(&ids[0]).await.unwrap();
    assert_eq!(
        rows.iter()
            .map(|r| r.passage_id.as_str())
            .collect::<Vec<_>>(),
        [stored[1].id.as_str(), stored[2].id.as_str()]
    );
    assert!(rows.iter().all(|r| r.start_line.is_some()));

    // An agent's document.
    let r = h
        .post(&t, "/api/context/ingest")
        .json(
            &json!({"name": "notes.md", "origin": {"origin": "agent", "name": "test"},
                      "text": "Axolotls regrow limbs.\n\nTardigrades survive vacuum."}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let doc: Document = r.json().await.unwrap();
    assert_eq!(doc.actor, "agent:test");
    assert_eq!(
        doc.origin,
        Origin::Agent {
            name: "test".into()
        }
    );
    let ready = eventually(|| async {
        let d = document(&h, &doc.id).await;
        matches!(d.status, DocumentStatus::Ready { .. }).then_some(d)
    })
    .await;
    let hits: Vec<Hit> = h
        .post(&t, "/api/context/search")
        .json(&json!({"text": "axolotls regrow", "scope": {"scope": "library"}, "k": 3}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(hits[0].document_id, doc.id);

    // Errors.
    let (status, e) = api_error(
        h.post(&t, "/api/context/search")
            .json(&json!({"text": "x", "scope": {"scope": "topic", "id": "nope"}, "k": 5}))
            .send()
            .await
            .unwrap(),
    )
    .await;
    assert_eq!((status, e.code.as_str()), (400, "bad_scope"));
    for path in ["outline", "passages?from=0&to=1"] {
        let (status, _) = api_error(
            h.get(&t, &format!("/api/context/documents/nope/{path}"))
                .send()
                .await
                .unwrap(),
        )
        .await;
        assert_eq!(status, 404, "{path}");
    }

    // Deleting it: 204, and the change feed lists it as deleted.
    let r = h
        .delete(&t, &format!("/api/context/documents/{}", doc.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 204);
    let changes: ContextChanges = h
        .get(&t, "/api/context/changes")
        .query(&[("since", ready.generation.to_string())])
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(changes.deleted, std::slice::from_ref(&doc.id));
    assert!(h.state.store.passages_of(&doc.id).await.unwrap().is_empty());
    let r = h
        .delete(&t, &format!("/api/context/documents/{}", doc.id))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 404);
}

// ---------------------------------------------------------------------------
// 4. A turn's traces
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_turn_that_searches_is_traced_under_one_id_without_content() {
    let (h, t) = signed_in().await;
    let bayes = upload_corpus(&h, &t, "bayes.md", None).await;
    let leaves = upload_corpus(&h, &t, "photosynthesis.md", None).await;
    wait_ready(&h, &t, &[&bayes.id, &leaves.id]).await;

    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[(
        "call_search",
        "search",
        &search_args("chlorophyll light reactions"),
    )])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["The pangolin answer."])));
    h.title_reply("Okapi musings");
    let d = chat_turn(&h, &t, "Tell me the okapi riddle", &[&bayes.id, &leaves.id]).await;
    let turn_id = d.items[0].turn_id.clone();
    let excerpt = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                card: Card::Passages(c),
                ..
            } => Some(c.passages[0].excerpt.clone()),
            _ => None,
        })
        .expect("a Passages card");
    let output_bytes = d
        .items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::ToolResult { output_bytes, .. } => Some(*output_bytes),
            _ => None,
        })
        .unwrap();

    let (rows, body) = traces(&h, &t, &[("trace_id", turn_id.clone())]).await;
    assert!(rows.iter().all(|r| r.trace_id == turn_id));
    let of = |k: TraceKind| rows.iter().filter(move |r| r.kind == k).collect::<Vec<_>>();
    let turn = of(TraceKind::Turn);
    assert_eq!(turn.len(), 1);
    let pipeline = h.state.pipeline.load_full().hash.clone();
    let calls = of(TraceKind::ModelCall);
    assert_eq!(calls.len(), 3, "two steps and the title: {calls:#?}");
    for c in &calls {
        assert_eq!(c.pipeline_hash.as_deref(), Some(pipeline.as_str()));
        assert!(c.prompt_version.is_some() && c.route.is_some());
        assert_eq!(c.provider.as_deref(), Some("openai"));
        assert!(c.usage.is_some() && c.cost_usd.is_some());
    }
    let tools = of(TraceKind::ToolCall);
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].tool.as_deref(), Some("search"));
    assert_eq!(tools[0].output_bytes, Some(output_bytes));
    let retrievals = of(TraceKind::Retrieval);
    assert_eq!(retrievals.len(), 1);
    assert_eq!(retrievals[0].cache_hit, Some(false));
    assert!(!retrievals[0].packed_ids.is_empty() && retrievals[0].query_hash.is_some());

    // Nothing the owner or the model wrote, no key, no tool output.
    let probe: String = excerpt.chars().take(40).collect();
    for secret in [
        "okapi",
        "Okapi",
        "pangolin",
        "chlorophyll",
        API_KEY,
        probe.as_str(),
    ] {
        assert!(!body.contains(secret), "{secret:?} is in a trace");
    }

    // By kind and window, in time order.
    let from = (Utc::now() - chrono::Duration::hours(1)).to_rfc3339();
    let to = (Utc::now() + chrono::Duration::hours(1)).to_rfc3339();
    let (by_kind, _) = traces(
        &h,
        &t,
        &[("kind", "model_call".into()), ("from", from), ("to", to)],
    )
    .await;
    assert!(by_kind.windows(2).all(|w| w[0].at <= w[1].at));
    assert!(by_kind.iter().all(|r| r.kind == TraceKind::ModelCall));
    let mine: Vec<&String> = by_kind
        .iter()
        .filter(|r| r.trace_id == turn_id)
        .map(|r| &r.id)
        .collect();
    assert_eq!(mine, calls.iter().map(|r| &r.id).collect::<Vec<_>>());

    // The hourly rollup sums every model call.
    let r = h
        .get(&t, "/api/traces/rollup")
        .query(&[("bucket", "hour")])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let hourly: Vec<TraceRollup> = r.json().await.unwrap();
    let model: Vec<&TraceRollup> = hourly
        .iter()
        .filter(|r| r.kind == TraceKind::ModelCall)
        .collect();
    assert_eq!(
        model.iter().map(|r| r.calls).sum::<u64>(),
        by_kind.len() as u64
    );
    assert_eq!(
        model.iter().map(|r| r.input).sum::<u64>(),
        by_kind.iter().map(|r| r.usage.unwrap().input).sum::<u64>()
    );
    assert_eq!(
        hourly
            .iter()
            .filter(|r| r.kind == TraceKind::Retrieval)
            .map(|r| r.calls)
            .sum::<u64>(),
        traces(&h, &t, &[("kind", "retrieval".into())])
            .await
            .0
            .len() as u64
    );
}

// ---------------------------------------------------------------------------
// 6. ⌘K on the FULLTEXT index
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn command_k_finds_lessons_and_notes_by_words_and_word_beginnings() {
    let (h, t) = signed_in().await;
    *h.generator.lesson_body.lock().unwrap() =
        Some("Orchids have zygomorphic flowers, mirror images on one axis only.".into());
    let proposal = h.propose(&t, &ask_request("Orchids", 1, false)).await;
    let r = h
        .post(&t, &format!("/api/proposals/{}/accept", proposal.id))
        .json(&json!({"outline": proposal.outline}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let course: Topic = r.json().await.unwrap();
    let lesson = eventually(|| async {
        library(&h, &t)
            .await
            .lessons
            .into_iter()
            .find(|l| l.topic_id == course.id && l.status == LessonStatus::Ready)
    })
    .await;
    let r = h
        .post(&t, "/api/notes")
        .json(
            &json!({"topic_id": course.id, "lesson_id": null, "title": "Practice",
                      "body_md": "The xylophonist tunes each bar by ear.", "quote": null}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let note: Note = r.json().await.unwrap();

    let search = |q: &'static str| {
        let h = &h;
        let t = &t;
        async move {
            let r = h
                .get(t, "/api/search")
                .query(&[("q", q)])
                .send()
                .await
                .unwrap();
            assert_eq!(r.status(), 200);
            r.json::<Vec<SearchHit>>().await.unwrap()
        }
    };
    for (q, word, kind, id) in [
        ("zygomorphic", "zygomorphic", SearchKind::Lesson, &lesson.id),
        ("zygom", "zygomorphic", SearchKind::Lesson, &lesson.id),
        ("xylophonist", "xylophonist", SearchKind::Note, &note.id),
        ("xylo", "xylophonist", SearchKind::Note, &note.id),
    ] {
        let hits = search(q).await;
        let hit = hits
            .iter()
            .find(|h| h.kind == kind && &h.id == id)
            .unwrap_or_else(|| panic!("{q}: {hits:?}"));
        assert!(hit.snippet.contains(word), "{q}: {:?}", hit.snippet);
        assert_eq!(hits.len(), 1, "{q}: {hits:?}");
    }
    assert!(search("zygomorphism").await.is_empty());
    // The lesson is also indexed for agents as a library document.
    eventually(|| async {
        let changes = margin_service::ingest::changes_since(&h.state, 0)
            .await
            .unwrap();
        changes
            .changed
            .iter()
            .any(|d| {
                d.origin
                    == Origin::Library {
                        kind: LibraryKind::Lesson,
                        id: lesson.id.clone(),
                    }
            })
            .then_some(())
    })
    .await;
}
