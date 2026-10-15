//! R2 end to end (`docs/r2-brief.md` §5): the listwise rerank on a lesson, a rerank
//! that fails, the agent tool reranked only by a variant, query rewriting, the prompt
//! catalog, and the cross-encoder's plumbing, through the real router with the
//! harness's fakes (`FakeGenerator` scripts the rerank and rewrite calls,
//! `FakeSourceTools` opens `FakeReranker` for a downloaded cross-encoder, the fake
//! Hugging Face serves its files). No real model, no network beyond localhost.

mod common;

use std::collections::HashMap;

use common::*;
use margin_agent::RetryPolicy;
use margin_agent::Scripted;
use margin_agent::scripted::{text_reply, tool_calls_reply};
use margin_ai::Ranked;
use margin_shared::context::{DocumentStatus, PromptInfo, TraceKind, TraceRow};
use margin_shared::pipeline::{PipelineVariants, RetrievalPurpose, Route};
use margin_shared::{
    ApiError, Card, ChatDetail, ChatItemKind, ChatState, EmbeddingModel, EmbeddingModelState,
    Event, Lesson, LessonStatus, Library, Provider, Source, SourceStatus, Topic, routes,
};
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Prices for the harness's model, so model calls are priced at write time.
const TEST_PRICES: &str = "[models.test-model]\ninput = 1.0\ncached_input = 0.5\noutput = 2.0\n";

/// Four paragraphs, one passage each (the fake chunker cuts on blank lines). The last
/// never says "gibbs": only a rewrite's added phrasings reach it from that word.
const NOTES: &str = "Fourier series write a periodic signal as a sum of sines.\n\n\
Gibbs phenomenon: a partial sum overshoots near a jump discontinuity.\n\n\
The coefficients come from integrals over one period.\n\n\
Ringing artefacts appear around sharp edges in compressed images.";

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

async fn api_error(r: reqwest::Response) -> (u16, ApiError) {
    let status = r.status().as_u16();
    (status, r.json().await.unwrap())
}

async fn library(h: &Harness, t: &str) -> Library {
    let r = h.get(t, "/api/library").send().await.unwrap();
    assert_eq!(r.status(), 200);
    r.json().await.unwrap()
}

/// `NOTES` uploaded (outside any topic) and read: the source and its passage ids in
/// paragraph order with their texts.
async fn notes(h: &Harness, t: &str) -> (Source, Vec<String>, HashMap<String, String>) {
    let r = h.upload(t, "signals.txt", None, NOTES).await;
    assert_eq!(r.status(), 200);
    let source: Source = r.json().await.unwrap();
    eventually(|| async {
        library(h, t)
            .await
            .sources
            .iter()
            .any(|s| s.id == source.id && matches!(s.status, SourceStatus::Ready { .. }))
            .then_some(())
    })
    .await;
    let mut rows = h.state.store.passages_of(&source.id).await.unwrap();
    rows.sort_by_key(|r| r.ord);
    assert_eq!(rows.len(), 4, "{rows:?}");
    let texts = rows
        .iter()
        .map(|r| (r.id.clone(), r.text.trim().to_string()))
        .collect();
    (source, rows.into_iter().map(|r| r.id).collect(), texts)
}

/// Replaces the pipeline in Settings (`PATCH /api/settings`); the status and body.
async fn set_pipeline(h: &Harness, t: &str, pipeline: Value) -> reqwest::Response {
    h.patch(t, "/api/settings")
        .json(&json!({ "pipeline": pipeline }))
        .send()
        .await
        .unwrap()
}

async fn pipeline_ok(h: &Harness, t: &str, pipeline: Value) {
    let r = set_pipeline(h, t, pipeline).await;
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
}

/// `GET /api/traces?trace_id=..`.
async fn traces_of(h: &Harness, t: &str, trace_id: &str) -> Vec<TraceRow> {
    assert!(
        h.state
            .tracer
            .flush(std::time::Duration::from_secs(5))
            .await
    );
    let r = h
        .get(t, "/api/traces")
        .query(&[("trace_id", trace_id)])
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    r.text()
        .await
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect()
}

fn retrievals(rows: &[TraceRow]) -> Vec<&TraceRow> {
    rows.iter()
        .filter(|r| r.kind == TraceKind::Retrieval)
        .collect()
}

fn calls_of<'a>(rows: &'a [TraceRow], version: &str) -> Vec<&'a TraceRow> {
    rows.iter()
        .filter(|r| r.kind == TraceKind::ModelCall && r.prompt_version.as_deref() == Some(version))
        .collect()
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

async fn wait_idle(h: &Harness, t: &str, chat_id: &str, items_before: usize) -> ChatDetail {
    eventually(|| async {
        let d = chat_detail(h, t, chat_id).await;
        (d.chat.state == ChatState::Idle && d.items.len() > items_before).then_some(d)
    })
    .await
}

/// The search tool's arguments with every field given (the schema is strict).
fn search_args(query: &str) -> String {
    json!({"query": query, "scope": null, "k": null, "modalities": null}).to_string()
}

/// One chat turn whose model calls `search` for `query` (as `call_id`) and answers.
fn script_search(h: &Harness, call_id: &str, query: &str) {
    h.generator.agent.push(Scripted::Events(tool_calls_reply(&[(
        call_id,
        "search",
        &search_args(query),
    )])));
    h.generator
        .agent
        .push(Scripted::Events(text_reply(&["Found it."])));
}

/// Starts a chat with `NOTES` attached whose first turn searches for `query`.
async fn chat_searching(
    h: &Harness,
    t: &str,
    source: &Source,
    call_id: &str,
    query: &str,
) -> ChatDetail {
    script_search(h, call_id, query);
    h.title_reply("Signals");
    let r = h
        .post(t, "/api/chats")
        .json(&json!({"message": {"text": "Look in my notes", "source_ids": [source.id]}}))
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

/// Another turn in `chat` whose model searches for `query`.
async fn search_again(
    h: &Harness,
    t: &str,
    chat_id: &str,
    call_id: &str,
    query: &str,
) -> ChatDetail {
    let before = chat_detail(h, t, chat_id).await.items.len();
    script_search(h, call_id, query);
    let r = h
        .post(t, &format!("/api/chats/{chat_id}/messages"))
        .json(&json!({"text": "Again"}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200, "{}", r.text().await.unwrap());
    wait_idle(h, t, chat_id, before).await
}

/// The passages the model got from `call_id`, as their texts in order.
fn search_output_texts(h: &Harness, call_id: &str) -> Vec<String> {
    let output = h
        .generator
        .agent
        .requests()
        .iter()
        .flat_map(|r| r.input.iter())
        .find(|i| i["type"] == "function_call_output" && i["call_id"] == call_id)
        .and_then(|i| i["output"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no output for {call_id}"));
    let v: Value = serde_json::from_str(&output).unwrap();
    v["passages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["text"].as_str().unwrap().trim().to_string())
        .collect()
}

/// The turn trace id of `call_id`'s turn (the turn's id).
fn turn_of(d: &ChatDetail, call_id: &str) -> String {
    d.items
        .iter()
        .find(|i| match &i.kind {
            ChatItemKind::Card { call_id: c, .. } => c == call_id,
            _ => false,
        })
        .map(|i| i.turn_id.clone())
        .unwrap_or_else(|| panic!("no card for {call_id}"))
}

fn passages_card(d: &ChatDetail, call_id: &str) -> Vec<margin_shared::Citation> {
    d.items
        .iter()
        .find_map(|i| match &i.kind {
            ChatItemKind::Card {
                call_id: c,
                card: Card::Passages(card),
            } if c == call_id => Some(card.passages.clone()),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Passages card for {call_id}"))
}

/// Proposes and accepts a one-lesson course on `source` (no videos, no quiz) and waits
/// for its lesson.
async fn lesson_on(h: &Harness, t: &str, source: &Source, prompt: &str) -> Lesson {
    let mut req = ask_request(prompt, 1, false);
    req.with_quiz = false;
    req.source_ids = vec![source.id.clone()];
    let p = h.propose(t, &req).await;
    let r = h
        .post(t, &format!("/api/proposals/{}/accept", p.id))
        .json(&json!({"outline": p.outline.unwrap(), "chips": {"with_videos": false, "with_quiz": false}}))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    let topic: Topic = r.json().await.unwrap();
    eventually(|| async {
        library(h, t)
            .await
            .lessons
            .into_iter()
            .find(|l| l.topic_id == topic.id && l.status == LessonStatus::Ready)
    })
    .await
}

/// Waits until every document is Ready and the index generation held still between
/// two looks, so a repeated retrieval has the same cache key.
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

async fn regenerate(h: &Harness, t: &str, lesson_id: &str) {
    index_settled(h).await;
    let n = h.generator.lesson_requests.lock().unwrap().len();
    eventually(|| async {
        let r = h
            .post(t, &format!("/api/lessons/{lesson_id}/regenerate"))
            .send()
            .await
            .unwrap();
        (r.status() == 200).then_some(())
    })
    .await;
    eventually(|| async {
        let ls = library(h, t).await.lessons;
        let l = ls.iter().find(|l| l.id == lesson_id).unwrap();
        (l.status == LessonStatus::Ready && h.generator.lesson_requests.lock().unwrap().len() > n)
            .then_some(())
    })
    .await;
}

fn ranked(grades: &[(&String, u8)]) -> Vec<Ranked> {
    grades
        .iter()
        .map(|(id, grade)| Ranked {
            id: id.to_string(),
            grade: *grade,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 1. A lesson reranked
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_lesson_is_reranked_traced_cached_and_records_what_wrote_it() {
    let (h, t) = signed_in().await;
    let (source, ids, texts) = notes(&h, &t).await;
    pipeline_ok(&h, &t, json!({"reranker": {"name": "listwise-small"}})).await;
    // The last paragraph first (grade 3); the first irrelevant (grade 0).
    *h.generator.rerank_reply.lock().unwrap() = Some(ranked(&[
        (&ids[3], 3),
        (&ids[2], 2),
        (&ids[1], 1),
        (&ids[0], 0),
    ]));
    *h.generator.lesson_body.lock().unwrap() = Some("From the notes.[^1]".into());
    let lesson = lesson_on(&h, &t, &source, "Fourier series").await;

    // The writer got the passages in the reranked order, the grade-0 one dropped.
    let ctx = h.generator.lesson_requests.lock().unwrap()[0].clone();
    assert!(!ctx.passages.is_empty());
    assert!(
        ctx.passages[0].text.contains(&texts[&ids[3]]),
        "{:?}",
        ctx.passages
    );
    assert!(
        ctx.passages
            .iter()
            .all(|p| !p.text.contains(&texts[&ids[0]])),
        "the grade-0 passage was dropped: {:?}",
        ctx.passages
    );

    // The lesson's rerank call (the last; the proposal's planner retrieval, purpose
    // `plan`, is reranked before it) on the Small route with the lesson's query.
    let calls = h.generator.rerank_requests.lock().unwrap().clone();
    let call = calls.last().expect("the lesson's rerank call");
    assert_eq!(call.opts.route, Route::Small);
    assert_eq!(call.opts.prompt_version.as_deref(), Some("rerank-v1"));
    assert!(
        call.query.starts_with(&lesson.title),
        "the lesson's title and summary come first: {}",
        call.query
    );
    assert_eq!(call.candidates.len(), 4);
    assert!(
        call.candidates
            .iter()
            .all(|c| c.label.contains("signals.txt"))
    );

    // Traced: the retrieval row names the reranker, the call is its child.
    let rows = traces_of(&h, &t, &lesson.id).await;
    let r = retrievals(&rows);
    assert_eq!(r.len(), 1);
    let retrieval = r[0];
    assert_eq!(retrieval.reranker.as_deref(), Some("listwise-small"));
    assert_eq!(retrieval.reranked, Some(call.candidates.len() as u32));
    assert_eq!(retrieval.error_code, None);
    assert!(retrieval.legs.as_ref().unwrap().rerank_ms > 0.0);
    let rerank = calls_of(&rows, "rerank-v1");
    assert_eq!(rerank.len(), 1);
    assert_eq!(rerank[0].route, Some(Route::Small));
    assert_eq!(rerank[0].job_id.as_deref(), Some(lesson.id.as_str()));
    assert_eq!(rerank[0].parent_id.as_deref(), Some(retrieval.id.as_str()));
    assert!(rerank[0].ok && rerank[0].cost_usd.is_some());

    // What wrote it, in the library: the lesson prompt, the writer's model and provider,
    // the pipeline its job's rows name.
    let with = lesson.written_with.clone().expect("written_with");
    assert_eq!(with.prompt_version, "lesson-v2");
    assert_eq!(with.model, "test-model");
    assert_eq!(with.provider, Provider::OpenAi);
    assert_eq!(
        Some(with.pipeline_hash.as_str()),
        retrieval.pipeline_hash.as_deref()
    );

    // Regenerating with the index unchanged reads the retrieval cache: no second rerank.
    // (Writing the lesson indexed it as a library document, which moved the index
    // generation, so the first regenerate refills the cache.)
    regenerate(&h, &t, &lesson.id).await;
    let after_first = h.generator.rerank_requests.lock().unwrap().len();
    let reranked_rows = calls_of(&traces_of(&h, &t, &lesson.id).await, "rerank-v1").len();
    regenerate(&h, &t, &lesson.id).await;
    assert_eq!(
        h.generator.rerank_requests.lock().unwrap().len(),
        after_first
    );
    let rows = traces_of(&h, &t, &lesson.id).await;
    let r = retrievals(&rows);
    assert_eq!(r.len(), 3);
    assert_eq!(r.last().unwrap().cache_hit, Some(true));
    assert_eq!(calls_of(&rows, "rerank-v1").len(), reranked_rows);
}

// ---------------------------------------------------------------------------
// 2. A rerank that fails is harmless
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_failed_rerank_keeps_the_fused_order_and_the_lesson_still_cites() {
    let (h, t) = signed_in().await;
    let (source, _ids, _texts) = notes(&h, &t).await;
    pipeline_ok(&h, &t, json!({"reranker": {"name": "listwise-small"}})).await;
    *h.generator.rerank_reply.lock().unwrap() = None;
    *h.generator.lesson_body.lock().unwrap() = Some("From the notes.[^1]".into());
    let lesson = lesson_on(&h, &t, &source, "Fourier series").await;

    assert!(!h.generator.rerank_requests.lock().unwrap().is_empty());
    let ctx = h.generator.lesson_requests.lock().unwrap()[0].clone();
    assert!(!ctx.passages.is_empty());
    assert_eq!(lesson.citations.len(), 1);
    assert_eq!(lesson.citations[0].source_id, source.id);

    let rows = traces_of(&h, &t, &lesson.id).await;
    let retrieval = retrievals(&rows)[0];
    assert_eq!(retrieval.error_code.as_deref(), Some("rerank_failed"));
    let rerank = calls_of(&rows, "rerank-v1");
    assert_eq!(rerank.len(), 1);
    assert!(!rerank[0].ok);
    assert_eq!(rerank[0].parent_id.as_deref(), Some(retrieval.id.as_str()));
}

// ---------------------------------------------------------------------------
// 3. The agent tool: not reranked by default, reranked by a variant
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_agent_search_is_reranked_only_when_the_variant_says_so() {
    let (h, t) = signed_in().await;
    let (source, ids, texts) = notes(&h, &t).await;
    pipeline_ok(&h, &t, json!({"reranker": {"name": "listwise-small"}})).await;
    *h.generator.rerank_reply.lock().unwrap() = Some(ranked(&[
        (&ids[2], 3),
        (&ids[3], 2),
        (&ids[1], 1),
        (&ids[0], 1),
    ]));

    let d = chat_searching(&h, &t, &source, "call_1", "fourier series sines").await;
    assert!(h.generator.rerank_requests.lock().unwrap().is_empty());
    let rows = traces_of(&h, &t, &turn_of(&d, "call_1")).await;
    let retrieval = retrievals(&rows)[0];
    assert_eq!(retrieval.reranked, None);
    assert!(calls_of(&rows, "rerank-v1").is_empty());

    pipeline_ok(
        &h,
        &t,
        json!({"reranker": {"name": "listwise-small", "params": {"apply_to": ["agent"]}}}),
    )
    .await;
    let d = search_again(&h, &t, &d.chat.id, "call_2", "fourier series sines").await;
    let calls = h.generator.rerank_requests.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].query, "fourier series sines");
    assert_eq!(calls[0].opts.route, Route::Small);
    assert_eq!(calls[0].choice.config.provider, d.chat.provider);
    // The model and the Passages card get the reranked order (adjacent passages are
    // packed together, in document order): the fused order put the first paragraph,
    // which names the query's words, first; the reranked one puts it last.
    let fused = search_output_texts(&h, "call_1");
    assert!(fused[0].contains(&texts[&ids[0]]), "{fused:?}");
    let got = search_output_texts(&h, "call_2");
    assert!(got[0].contains(&texts[&ids[2]]), "{got:?}");
    assert_eq!(got.last(), Some(&texts[&ids[0]]), "{got:?}");
    let card = passages_card(&d, "call_2");
    assert_eq!(card.len(), got.len());
    assert!(
        texts[&ids[0]].starts_with(card.last().unwrap().excerpt.trim_end_matches('…')),
        "{card:?}"
    );
    let rows = traces_of(&h, &t, &turn_of(&d, "call_2")).await;
    let retrieval = retrievals(&rows)[0];
    assert_eq!(retrieval.reranker.as_deref(), Some("listwise-small"));
    assert_eq!(retrieval.reranked, Some(4));
    let rerank = calls_of(&rows, "rerank-v1");
    assert_eq!(rerank.len(), 1);
    assert_eq!(rerank[0].parent_id.as_deref(), Some(retrieval.id.as_str()));
}

// ---------------------------------------------------------------------------
// 4. Rewriting
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn a_short_search_is_rewritten_once_and_thin_lesson_plans_are_too() {
    let (h, t) = signed_in().await;
    let (source, ids, texts) = notes(&h, &t).await;
    pipeline_ok(&h, &t, json!({"rewriter": {"name": "small-expand-v1"}})).await;
    *h.generator.rewrite_reply.lock().unwrap() = Some(vec![
        "ringing artefacts sharp edges".into(),
        "overshoot near a jump".into(),
    ]);

    // One word: rewritten, for the agent, with the chat's document names.
    let d = chat_searching(&h, &t, &source, "call_1", "gibbs").await;
    let calls = h.generator.rewrite_requests.lock().unwrap().clone();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].input.query, "gibbs");
    assert_eq!(calls[0].input.purpose, RetrievalPurpose::Agent);
    assert!(
        calls[0]
            .input
            .scope_names
            .iter()
            .any(|n| n.contains("signals.txt")),
        "{:?}",
        calls[0].input.scope_names
    );
    assert_eq!(calls[0].opts.route, Route::Small);
    assert_eq!(calls[0].opts.prompt_version.as_deref(), Some("rewrite-v1"));
    let rows = traces_of(&h, &t, &turn_of(&d, "call_1")).await;
    let retrieval = retrievals(&rows)[0];
    assert_eq!(retrieval.queries, Some(3));
    let rewrite = calls_of(&rows, "rewrite-v1");
    assert_eq!(rewrite.len(), 1);
    assert_eq!(rewrite[0].route, Some(Route::Small));
    assert_eq!(rewrite[0].parent_id.as_deref(), Some(retrieval.id.as_str()));
    // The passage only the added phrasings name is found.
    let got = search_output_texts(&h, "call_1");
    assert!(got.iter().any(|g| g.contains(&texts[&ids[3]])), "{got:?}");

    // The same query again in the chat: no second rewrite (the rewrite cache). (Each
    // turn re-indexes the chat itself, which moves the index generation, so a chat's
    // repeated search isn't a retrieval cache hit; the context API's is, below.)
    let d = search_again(&h, &t, &d.chat.id, "call_2", "gibbs").await;
    assert_eq!(h.generator.rewrite_requests.lock().unwrap().len(), 1);
    let rows = traces_of(&h, &t, &turn_of(&d, "call_2")).await;
    assert_eq!(retrievals(&rows)[0].queries, Some(3));

    // The context API (purpose `context`): its first search rewrites (another purpose,
    // so another rewrite), the same search again is a retrieval cache hit with no call.
    index_settled(&h).await;
    let search = || async {
        let r = h
            .post(&t, routes::CONTEXT_SEARCH)
            .header("x-margin-agent", "r2-e2e")
            .json(&json!({"text": "gibbs", "scope": {"scope": "sources", "ids": [source.id]}, "k": 8}))
            .send()
            .await
            .unwrap();
        assert_eq!(r.status(), 200);
    };
    search().await;
    let calls = h.generator.rewrite_requests.lock().unwrap().clone();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[1].input.purpose, RetrievalPurpose::Context);
    search().await;
    assert_eq!(h.generator.rewrite_requests.lock().unwrap().len(), 2);
    assert!(
        h.state
            .tracer
            .flush(std::time::Duration::from_secs(5))
            .await
    );
    let all = margin_service::trace::read_rows(
        &h.state.store,
        None,
        chrono::DateTime::UNIX_EPOCH,
        chrono::Utc::now() + chrono::Duration::minutes(1),
        None,
    )
    .await
    .unwrap();
    let last = all
        .iter()
        .filter(|r| r.kind == TraceKind::Retrieval)
        .max_by_key(|r| r.at)
        .unwrap();
    assert_eq!(last.cache_hit, Some(true), "{last:?}");

    // Five words: no call.
    let d = search_again(
        &h,
        &t,
        &d.chat.id,
        "call_3",
        "how do the fourier coefficients come",
    )
    .await;
    assert_eq!(h.generator.rewrite_requests.lock().unwrap().len(), 2);
    let rows = traces_of(&h, &t, &turn_of(&d, "call_3")).await;
    assert_eq!(retrievals(&rows)[0].queries, Some(1));

    // A course's first lesson has a full plan (title and summary, the summary, the
    // course's request): no call.
    let lesson = lesson_on(&h, &t, &source, "Signals").await;
    let rows = traces_of(&h, &t, &lesson.id).await;
    assert!(calls_of(&rows, "rewrite-v1").is_empty());
    assert_eq!(retrievals(&rows)[0].queries, Some(3));

    // A lesson the agent adds with `write_lesson` (no proposal; two strings): one call,
    // as a lesson. The tool acts as the chat's owner (L1), so it runs in the chat above.
    let before = h.generator.rewrite_requests.lock().unwrap().len();
    let registry = margin_service::agent_tools::registry(&h.state);
    let tool = registry.get("write_lesson").unwrap().clone();
    let out = tool
        .run(
            &tool_ctx(&d.chat.id),
            json!({
                "topic_id": lesson.topic_id,
                "title": "Gibbs",
                "summary": "Overshoot",
                "minutes": 5,
                "level": "intro",
                "with_quiz": false,
            }),
        )
        .await;
    assert!(out.ok, "{}", out.output);
    let id = serde_json::from_str::<Value>(&out.output).unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    eventually(|| async {
        library(&h, &t)
            .await
            .lessons
            .into_iter()
            .find(|l| l.id == id && l.status == LessonStatus::Ready)
    })
    .await;
    let calls = h.generator.rewrite_requests.lock().unwrap().clone();
    assert_eq!(calls.len(), before + 1);
    assert_eq!(calls[before].input.purpose, RetrievalPurpose::Lesson);
    assert_eq!(calls[before].input.query, "Gibbs Overshoot");
    let rows = traces_of(&h, &t, &id).await;
    let rewrite = calls_of(&rows, "rewrite-v1");
    assert_eq!(rewrite.len(), 1);
    assert_eq!(rewrite[0].job_id.as_deref(), Some(id.as_str()));
}

fn tool_ctx(chat_id: &str) -> margin_agent::ToolContext {
    margin_agent::ToolContext {
        chat_id: chat_id.into(),
        turn_id: "turn".into(),
        call_id: "call".into(),
        origin_device: margin_shared::DeviceRef {
            device_id: "d".into(),
            name: "Mac".into(),
        },
        workspaces: Vec::new(),
        devices: std::sync::Arc::new(margin_agent::NoDevices),
    }
}

// ---------------------------------------------------------------------------
// 5. The prompt catalog
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_prompt_catalog_lists_every_prompt_the_service_sends() {
    let (h, t) = signed_in().await;
    let r = h.get(&t, routes::PIPELINE_PROMPTS).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let catalog: Vec<PromptInfo> = r.json().await.unwrap();
    for job in [
        "lesson", "scene", "plan", "rerank", "rewrite", "agent", "title", "condense",
    ] {
        let p = catalog
            .iter()
            .find(|p| p.job == job)
            .unwrap_or_else(|| panic!("{job} missing from the catalog"));
        assert!(p.version.starts_with(job), "{job}: {}", p.version);
        assert_eq!(p.constant_hash.len(), 16, "{job}");
        assert!(p.constant_chars > 0, "{job}");
    }
    assert_eq!(
        catalog
            .iter()
            .find(|p| p.job == "condense")
            .unwrap()
            .version,
        "condense-v2"
    );
}

// ---------------------------------------------------------------------------
// 6. The cross-encoder's plumbing
// ---------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn the_cross_encoder_needs_its_download_then_reranks_the_agent_search() {
    let (h, t) = signed_in().await;
    let (source, ids, texts) = notes(&h, &t).await;

    // Not downloaded: the variant is unavailable and Settings refuses it.
    let v: PipelineVariants = h
        .get(&t, routes::PIPELINE_VARIANTS)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let xenc = v.variants.iter().find(|n| n.name == "rerank-xenc").unwrap();
    assert!(
        xenc.unavailable
            .as_deref()
            .is_some_and(|w| w.contains("download the reranker model")),
        "{:?}",
        xenc.unavailable
    );
    let r = set_pipeline(&h, &t, json!({"reranker": {"name": "onnx-crossencoder"}})).await;
    let (status, e) = api_error(r).await;
    assert_eq!(status, 400);
    assert_eq!(e.code, "bad_pipeline");

    // Download it from the (fake) Hugging Face, with progress events.
    h.hf.has_reranker
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let mut socket = h.socket(&t).await;
    let r = h
        .post(&t, routes::RERANKER_MODEL_DOWNLOAD)
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    socket
        .until(|e| {
            matches!(e, Event::EmbeddingModelChanged { model }
                if matches!(model.reranker.as_ref().map(|r| &r.state), Some(EmbeddingModelState::Ready { .. })))
        })
        .await;
    let m: EmbeddingModel = h
        .get(&t, "/api/embedding")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(matches!(
        m.reranker.unwrap().state,
        EmbeddingModelState::Ready { .. }
    ));

    // Now Settings takes it, and an agent search is reranked by the model (here
    // `FakeReranker`: shared words with the query).
    pipeline_ok(&h, &t, json!({"reranker": {"name": "onnx-crossencoder"}})).await;
    let d = chat_searching(&h, &t, &source, "call_1", "ringing sharp edges images").await;
    assert_eq!(
        h.source_tools
            .reranker_loads
            .load(std::sync::atomic::Ordering::SeqCst),
        1
    );
    let got = search_output_texts(&h, "call_1");
    assert!(got[0].contains(&texts[&ids[3]]), "{got:?}");
    let rows = traces_of(&h, &t, &turn_of(&d, "call_1")).await;
    let retrieval = retrievals(&rows)[0];
    assert_eq!(retrieval.reranker.as_deref(), Some("onnx-crossencoder"));
    assert!(retrieval.reranked.is_some_and(|n| n > 0));
    assert!(retrieval.legs.as_ref().unwrap().rerank_ms > 0.0);
    assert_eq!(retrieval.error_code, None);
    // No small-model call: the cross-encoder runs here.
    assert!(calls_of(&rows, "rerank-v1").is_empty());

    // Removing the download: the next rerank keeps the fused order (traced), and
    // Settings no longer offers the variant.
    let r = h.delete(&t, routes::RERANKER_MODEL).send().await.unwrap();
    assert_eq!(r.status(), 200);
    let m: EmbeddingModel = r.json().await.unwrap();
    assert!(matches!(
        m.reranker.unwrap().state,
        EmbeddingModelState::NotDownloaded { .. }
    ));
    let v: PipelineVariants = h
        .get(&t, routes::PIPELINE_VARIANTS)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let xenc = v.variants.iter().find(|n| n.name == "rerank-xenc").unwrap();
    assert!(xenc.unavailable.is_some());
}
