//! The CI regression gate (`docs/agent-rag-architecture.md` §3.3, §6.4 e2e 7):
//! `cargo test -p margin-evals`. Recorded fixtures and in-memory SurrealDB with the
//! service's real indexes only; no model API, no Hugging Face, no real embedding model.

use std::path::PathBuf;

use margin_evals::Gates;
use margin_evals::ci::run_gates;
use margin_evals::corpus::{EXTRACTED_FILE, Extraction};
use margin_evals::gates::{BASELINE_FILE, Baseline};
use margin_evals::golden::evals_dir;
use margin_evals::live::recording_file;
use margin_evals::recorded::{EMBEDDINGS_FILE, RecordedVectors};
use margin_evals::runner::{Env, Paths};

/// The real pipeline over the recorded corpus against the committed baseline (§6.4
/// e2e 7): `sections-v2`, `hybrid-rrf`/`rrf`/`mmr-v1` and `pack_budget` on in-memory
/// SurrealDB with the service's real indexes, the vectors replayed from the recording
/// of the service's embedder (EmbeddingGemma 2 on ONNX Runtime, 8-bit, the default
/// backend), never a model. Recorded by hand with `margin-eval record-embeddings`
/// (`evals/recorded/embeddings.bin` and `evals/recorded/extracted.json`) and
/// `margin-eval run --suite retrieval --variant baseline --write-baseline`
/// (`evals/baseline.json`), committed together.
///
/// Citations are gated once generations are recorded (`margin-eval record-generations`
/// calls the writing model with the owner's key): until
/// `evals/recorded/generations/<prompt version>/` exists that gate is skipped and every
/// other gate runs.
#[tokio::test]
async fn ci_gates_on_the_recorded_corpus() {
    // HNSW assigns node levels from a random generator, seeded from entropy unless told
    // otherwise; with the corpus this small and `ef` this wide the search is exhaustive
    // either way, but a fixed seed makes the graph, and so the gate, the same on every
    // run. `SURREAL_HNSW_BUILD_SEED` in the environment still wins.
    surrealdb_cnf::set_default_hnsw_build_seed(0x4d61_7267_696e);
    let evals = evals_dir();
    let backend = margin_embed::Backend::default();
    assert_eq!(recording_file(backend), PathBuf::from(EMBEDDINGS_FILE));
    let vectors = RecordedVectors::open(&evals.join(EMBEDDINGS_FILE)).unwrap();
    let baseline = Baseline::load(&evals.join(BASELINE_FILE)).unwrap();
    assert_eq!(
        baseline.embedder_version,
        backend.embedder_version(),
        "the baseline is measured on the service's embedder"
    );
    let gates: Gates =
        toml::from_str(&std::fs::read_to_string(evals.join("gates.toml")).unwrap()).unwrap();
    let env = Env {
        models: None,
        paths: Paths::same(&evals),
        vectors: &vectors,
        extraction: Extraction::load(&evals.join(EXTRACTED_FILE)).unwrap(),
        register: &|_| {},
        pack_budget: &margin_service::retrieval::pack_budget,
    };
    // Wall-clock latency is gated only when asked (`MARGIN_EVAL_GATE_LATENCY=1`, on an
    // idle machine): the CI runners share their host with other VMs and builds, and a
    // loaded Mac measured p95 at 3.8× the baseline with identical rankings.
    let mut baseline = baseline;
    if std::env::var("MARGIN_EVAL_GATE_LATENCY").as_deref() != Ok("1") {
        baseline.retrieval.latency_p95_ms = 0.0;
    }
    let outcome = run_gates(&env, &gates, &baseline, false).await.unwrap();
    let r = &outcome.retrieval;
    eprintln!(
        "recall@5 {:.1}, recall@10 {:.1}, MRR {:.1}, nDCG@10 {:.1}, negatives FP {:.1} %, p95 {:.1} ms",
        r.overall.recall_at_5,
        r.overall.recall_at_10,
        r.overall.mrr,
        r.overall.ndcg_at_10,
        r.negative_fp_rate,
        r.latency_p95_ms
    );
    assert!(
        outcome.violations.is_empty(),
        "the eval gates failed:\n{}",
        outcome.violations.join("\n")
    );
}

/// The R2 variants over the recorded corpus (`docs/r2-brief.md` §6): `rerank-small` and
/// `rewrite-small` with their small-model stages replayed from
/// `evals/recorded/{reranks,rewrites}.jsonl` (recorded by hand with the owner's key:
/// `margin-eval record-reranks`, `record-rewrites`, then `record-embeddings`), checked
/// against `[rerank]` in `evals/gates.toml` (top-10 places changed, fallbacks) and the
/// tool-output size. A variant whose recording isn't committed yet is skipped with a
/// message saying so; a recording without the vectors it needs fails.
#[tokio::test]
async fn r2_variants_on_the_recorded_small_calls() {
    use margin_evals::ci::run_small_gates;
    use margin_evals::small::SmallKind;
    let evals = evals_dir();
    let gates: Gates =
        toml::from_str(&std::fs::read_to_string(evals.join("gates.toml")).unwrap()).unwrap();
    let mut violations = Vec::new();
    for (variant, kind) in [
        ("rerank-small", SmallKind::Reranks),
        ("rewrite-small", SmallKind::Rewrites),
    ] {
        let recording = evals.join(kind.file());
        if !recording.is_file() {
            eprintln!(
                "SKIPPED the {variant} gate: {} isn't recorded yet (the owner runs `margin-eval \
                 {} --variant {variant}`, then `margin-eval record-embeddings`, and commits \
                 both)",
                recording.display(),
                kind.command()
            );
            continue;
        }
        let backend = margin_embed::Backend::Onnx(margin_embed::OnnxDtype::Q8);
        let embeddings = evals.join(margin_evals::live::recording_file(backend));
        let vectors = RecordedVectors::open(&embeddings).unwrap_or_else(|e| {
            panic!(
                "{} is recorded but the vectors it replays over aren't ({e:#}): run `margin-eval \
                 record-embeddings` and commit it",
                recording.display()
            )
        });
        let env = Env {
            models: None,
            paths: Paths::same(&evals),
            vectors: &vectors,
            extraction: Extraction::load(&evals.join(EXTRACTED_FILE)).unwrap(),
            register: &|_| {},
            pack_budget: &margin_service::retrieval::pack_budget,
        };
        let o = run_small_gates(&env, &gates, variant).await.unwrap();
        let (r, s) = (&o.retrieval, &o.retrieval.stages);
        eprintln!(
            "{variant}: recall@10 {:.1}, nDCG@10 {:.1}, top-10 moved {:.1} %, fallbacks {:.1} %, \
             {:.2} queries per search, rerank p95 {:.0} ms, rewrite p95 {:.0} ms, ${:.5} per query",
            r.overall.recall_at_10,
            r.overall.ndcg_at_10,
            s.rerank_moved_at_10,
            s.rerank_fallback_rate,
            s.queries_mean,
            s.rerank_ms_p95,
            s.rewrite_ms_p95,
            s.small_cost_usd
        );
        violations.extend(o.violations);
    }
    assert!(
        violations.is_empty(),
        "the R2 gates failed:\n{}",
        violations.join("\n")
    );
}

/// The prompt lock (R2, `docs/r2-brief.md` D4.2): every prompt's constant block hashes
/// as `evals/prompts.lock` says under its version. Needs no recording; always on
/// (`[prompts] lock` in `evals/gates.toml`; off, it only prints what differs).
#[test]
fn prompt_lock() {
    use margin_evals::gates::{PROMPT_LOCK_FILE, PromptLock, check_prompt_lock, locked_catalog};
    let path = evals_dir().join(PROMPT_LOCK_FILE);
    let lock: PromptLock = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    let violations = check_prompt_lock(&lock, &locked_catalog());
    let gates: Gates =
        toml::from_str(&std::fs::read_to_string(evals_dir().join("gates.toml")).unwrap()).unwrap();
    if !gates.prompts.lock {
        for v in &violations {
            eprintln!("prompt lock (advisory): {v}");
        }
        return;
    }
    assert!(
        violations.is_empty(),
        "evals/prompts.lock disagrees with the prompts:\n{}",
        violations.join("\n")
    );
}

// ---------------------------------------------------------------------------
// The Claude prompt-prefix gate (docs/agent-providers-brief.md D12)
// ---------------------------------------------------------------------------

mod claude_prefix {
    use std::convert::Infallible;
    use std::sync::{Arc, Mutex};

    use axum::Router;
    use axum::body::{Body, Bytes};
    use axum::extract::State;
    use axum::http::{HeaderMap, header};
    use axum::response::Response;
    use axum::routing::post;
    use margin_ai::prompts::{self, Prompt};
    use margin_ai::{
        CallOptions, ClaudeClient, DocumentContents, FailedScene, Image, LessonContext, Passage,
        PlanRequest, RerankCandidate, RewriteInput, RouteModels, SceneRequest, TextModel,
    };
    use margin_evals::prefix::{check_call, check_recorded, identical_prefixes, known_constants};
    use margin_shared::pipeline::RetrievalPurpose;
    use margin_shared::pipeline::Route;
    use margin_shared::{Level, OutlineLesson};
    use serde_json::{Value, json};

    /// A fake Messages API: records every request body as the client sent it (with its
    /// `x-api-key`) and answers each with one streamed text block.
    #[derive(Default)]
    struct FakeMessages {
        requests: Mutex<Vec<(Option<String>, Value)>>,
    }

    fn sse(event: &str, data: Value) -> String {
        format!("event: {event}\ndata: {data}\n\n")
    }

    /// A complete, documented Messages stream answering `text`.
    fn text_stream(model: &str, text: &str) -> Vec<String> {
        vec![
            sse(
                "message_start",
                json!({"type": "message_start", "message": {
                    "id": "msg_fake", "type": "message", "role": "assistant", "model": model,
                    "content": [], "stop_reason": null, "stop_sequence": null,
                    "usage": {"input_tokens": 1200, "cache_read_input_tokens": 0,
                              "cache_creation_input_tokens": 0, "output_tokens": 1}}}),
            ),
            sse(
                "content_block_start",
                json!({"type": "content_block_start", "index": 0,
                       "content_block": {"type": "text", "text": ""}}),
            ),
            sse("ping", json!({"type": "ping"})),
            sse(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": 0,
                       "delta": {"type": "text_delta", "text": text}}),
            ),
            sse(
                "content_block_stop",
                json!({"type": "content_block_stop", "index": 0}),
            ),
            sse(
                "message_delta",
                json!({"type": "message_delta",
                       "delta": {"stop_reason": "end_turn", "stop_sequence": null},
                       "usage": {"output_tokens": 12}}),
            ),
            sse("message_stop", json!({"type": "message_stop"})),
        ]
    }

    async fn messages(
        State(fake): State<Arc<FakeMessages>>,
        headers: HeaderMap,
        body: Bytes,
    ) -> Response {
        let key = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let json: Value = serde_json::from_slice(&body).expect("the client sends JSON");
        let model = json["model"].as_str().unwrap_or("?").to_string();
        fake.requests.lock().unwrap().push((key, json));
        let chunks = text_stream(&model, "## Idea\n\nText.\n");
        let stream = futures_util::stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, Infallible>(Bytes::from(c))),
        );
        Response::builder()
            .header(header::CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(stream))
            .unwrap()
    }

    async fn spawn() -> (ClaudeClient, Arc<FakeMessages>) {
        let fake = Arc::new(FakeMessages::default());
        let app = Router::new()
            .route("/v1/messages", post(messages))
            .with_state(fake.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = ClaudeClient::new(
            format!("http://{addr}"),
            "test-key",
            RouteModels::claude_default(),
        );
        (client, fake)
    }

    fn lesson_ctx(index: usize, with_quiz: bool) -> LessonContext {
        LessonContext {
            subject_title: "Signals & Systems".into(),
            topic_title: "Fourier series".into(),
            prompt: "Fourier series".into(),
            level: if with_quiz {
                Level::Advanced
            } else {
                Level::Intro
            },
            with_quiz,
            outline: vec![
                OutlineLesson {
                    title: "Waves as sums".into(),
                    summary: "Why sums of sines.".into(),
                    minutes: 8,
                },
                OutlineLesson {
                    title: "Coefficients".into(),
                    summary: "Computing them.".into(),
                    minutes: 12,
                },
                OutlineLesson {
                    title: "Convergence".into(),
                    summary: "Gibbs.".into(),
                    minutes: 10,
                },
            ],
            index,
            passages: vec![],
        }
    }

    fn passages() -> Vec<Passage> {
        vec![Passage {
            source: "dsp.pdf".into(),
            page: Some(12),
            text: "The coefficient a_n is the projection onto cos(n x).".into(),
            ..Default::default()
        }]
    }

    /// Varied calls of every constant-first prompt the Claude client's `TextModel`
    /// runs: lessons (first, middle, last; intro and advanced; with and without quiz,
    /// passages and a picture), scenes (first try and repair), the planner, the
    /// reranker, the query rewriter.
    fn prompts_sent() -> Vec<Prompt> {
        let mut out = Vec::new();
        for index in 0..3 {
            for with_quiz in [false, true] {
                let mut ctx = lesson_ctx(index, with_quiz);
                if index > 0 {
                    ctx.passages = passages();
                }
                if index == 2 && with_quiz {
                    ctx.passages.push(Passage {
                        source: "board.jpg".into(),
                        image: Some(Image {
                            media_type: "image/jpeg".into(),
                            data: vec![0xff, 0xd8, 0xff],
                        }),
                        ..Default::default()
                    });
                }
                out.push(prompts::lesson(&ctx));
            }
        }
        out.push(prompts::scene(&SceneRequest {
            lesson_title: "Waves as sums".into(),
            lesson_body_md: "## Idea\n\nSines add.\n\n## Check yourself\n\nQ".into(),
            previous_attempt: None,
        }));
        out.push(prompts::scene(&SceneRequest {
            lesson_title: "Coefficients".into(),
            lesson_body_md: "## Idea\n\nProject.".into(),
            previous_attempt: Some(FailedScene {
                code: "class S(Scene): pass".into(),
                error: "NameError".into(),
            }),
        }));
        for (count, with_quiz) in [(2, true), (5, false)] {
            out.push(prompts::plan(&PlanRequest {
                prompt: "Fourier series, for signal processing".into(),
                level: Level::Intermediate,
                lesson_count: count,
                with_quiz,
                draft: None,
                contents: vec![DocumentContents {
                    document: "dsp.pdf".into(),
                    sections: vec!["2 Fourier series > 2.2 Coefficients".into()],
                }],
                survey: passages(),
            }));
        }
        for query in ["what is a_n", "Gibbs phenomenon"] {
            out.push(prompts::rerank(
                query,
                &[
                    RerankCandidate {
                        id: "c1".into(),
                        label: "dsp.pdf, p. 12".into(),
                        text: "The coefficient a_n is a projection.".into(),
                    },
                    RerankCandidate {
                        id: "c2".into(),
                        label: "dsp.pdf, p. 14".into(),
                        text: "Overshoot near a jump.".into(),
                    },
                ],
            ));
        }
        for (query, purpose, scope_names) in [
            (
                "gibbs",
                RetrievalPurpose::Agent,
                vec!["dsp.pdf".to_string()],
            ),
            (
                "Fourier coefficients",
                RetrievalPurpose::Lesson,
                vec!["dsp.pdf".to_string(), "notes/waves.md".to_string()],
            ),
        ] {
            out.push(prompts::rewrite(&RewriteInput {
                query: query.into(),
                purpose,
                scope_names,
                max_added: 2,
            }));
        }
        out
    }

    /// The route a prompt's calls run on (D10: lessons, scenes and the planner on
    /// Write; the reranker and the rewriter are small tasks).
    fn route(p: &Prompt) -> Route {
        if p.job == "rerank" || p.job == "rewrite" {
            Route::Small
        } else {
            Route::Write
        }
    }

    /// D12's invariant on the bodies the fake Messages server recorded: for one prompt
    /// version, `system[0]` is that version's constant block byte for byte with the
    /// ephemeral marker, `system[1]` is the variable part (unmarked; absent when there
    /// is none), one breakpoint per text call, the constant block nowhere else; and
    /// each call is on its route's model.
    ///
    /// Drives the real `ClaudeClient` against the fake Messages server.
    #[tokio::test]
    async fn claude_prefixes_are_byte_identical_per_prompt_version() {
        let (client, fake) = spawn().await;
        let sent = prompts_sent();
        for p in &sent {
            let opts = CallOptions::new(route(p)).keyed(p);
            let out = client
                .stream_parts(&p.instructions(), &p.input, &opts, Box::new(|_| {}))
                .await
                .unwrap();
            assert_eq!(out.text, "## Idea\n\nText.\n");
        }
        let recorded: Vec<(Option<String>, Value)> = fake.requests.lock().unwrap().clone();
        assert_eq!(recorded.len(), sent.len());
        let bodies: Vec<Value> = recorded.iter().map(|(_, b)| b.clone()).collect();
        let known = known_constants();
        let report = check_recorded(&bodies, &known);
        assert!(
            report.violations.is_empty(),
            "the Claude prefix gate failed:\n{}",
            report.violations.join("\n")
        );
        assert_eq!(report.per_version[prompts::LESSON_VERSION], 6);
        assert_eq!(report.per_version[prompts::SCENE_VERSION], 2);
        assert_eq!(report.per_version[prompts::PLAN_VERSION], 2);
        assert_eq!(report.per_version[prompts::RERANK_VERSION], 2);
        assert_eq!(report.per_version[prompts::REWRITE_VERSION], 2);
        let models = RouteModels::claude_default();
        for (i, (p, body)) in sent.iter().zip(&bodies).enumerate() {
            let v = check_call(body, p);
            assert!(v.is_empty(), "request {i} ({}): {v:?}", p.version);
            assert_eq!(
                body["model"],
                models.get(route(p)),
                "request {i}: the route's model"
            );
        }
        for (version, _) in &known {
            let same: Vec<&Value> = sent
                .iter()
                .zip(&bodies)
                .filter(|(p, _)| p.version == *version)
                .map(|(_, b)| b)
                .collect();
            assert!(identical_prefixes(same), "{version}");
        }
        // The variable parts really differ, so the identity isn't vacuous.
        let rest = |i: usize| bodies[i]["system"][1]["text"].clone();
        assert_ne!(rest(0), rest(5));
        assert!(
            recorded
                .iter()
                .all(|(key, _)| key.as_deref() == Some("test-key")),
            "x-api-key on every call"
        );
    }

    /// The gate itself on bodies shaped as the brief says the client sends them, so it
    /// runs on every branch: a pass, then a client that marks the whole instructions
    /// (the variable part cached too), which it must catch.
    #[test]
    fn the_gate_on_brief_shaped_bodies() {
        let sent = prompts_sent();
        let body = |p: &Prompt, merged: bool| {
            let ins = p.instructions();
            let (constant, rest) = ins.split_at(p.constant.len());
            let first = if merged { ins.as_str() } else { constant };
            let mut system = vec![
                json!({"type": "text", "text": first, "cache_control": {"type": "ephemeral"}}),
            ];
            if !merged && !rest.is_empty() {
                system.push(json!({"type": "text", "text": rest}));
            }
            json!({
                "model": "claude-opus-5-5",
                "max_tokens": 64000,
                "stream": true,
                "system": system,
                "messages": [{"role": "user", "content": [{"type": "text", "text": p.input_text()}]}],
            })
        };
        let good: Vec<Value> = sent.iter().map(|p| body(p, false)).collect();
        let report = check_recorded(&good, &known_constants());
        assert_eq!(report.violations, Vec::<String>::new());
        assert_eq!(report.calls, sent.len());
        assert!(
            sent.iter()
                .zip(&good)
                .all(|(p, b)| check_call(b, p).is_empty())
        );
        let bad = body(&sent[1], true);
        let report = check_recorded(std::slice::from_ref(&bad), &known_constants());
        assert!(
            report.violations[0].contains("breakpoint sits after variable text"),
            "{:?}",
            report.violations
        );
    }
}

/// The access gates (L2, `docs/l2-brief.md` D8, `[access]` in `evals/gates.toml`):
/// `margin-eval run --suite access` on the baseline variant. The fixture corpus is stamped
/// with the audiences of `evals/access.toml` through the service's own writer; every
/// golden query is searched as every principal there (expanded by the service's own
/// functions) and every hit checked against the oracle (the fixture and the expansion, in
/// Rust, independent of the SQL); every by-id read the oracle forbids must come back
/// empty; the owner's ranked ids must equal the same statements' without the reader's
/// conjuncts, with the p95 within the ratio; a principal who reads every relevant
/// document sees the relevant hits in the owner's order.
///
/// Never ignored: on the recorded vectors when they are committed, else on the
/// deterministic `FakeEmbedder` (leakage is a property of the predicate, not of the
/// vectors). It fails if a prefix rule comes back (a reader in `company` reading
/// `company/engineering`).
#[tokio::test]
async fn access_gates() {
    use margin_evals::corpus::documents;
    use margin_evals::recorded::{FakeVectors, Vectors};
    use margin_evals::runner::run_access;
    use margin_evals::store::EvalStore;

    let evals = evals_dir();
    let corpus = evals.join("corpus");
    let docs = documents(&corpus).unwrap();
    let recorded = RecordedVectors::open(&evals.join(EMBEDDINGS_FILE)).ok();
    let fake = FakeVectors;
    let vectors: &dyn Vectors = match &recorded {
        Some(r) => r,
        None => {
            eprintln!(
                "access: no {EMBEDDINGS_FILE} yet, so on the deterministic FakeEmbedder \
                 (bag of words)"
            );
            &fake
        }
    };
    let extraction = Extraction::load(&evals.join(EXTRACTED_FILE))
        .unwrap_or_else(|_| Extraction::fixture(&corpus, &docs).unwrap());
    let env = Env {
        models: None,
        paths: Paths::same(&evals),
        vectors,
        extraction,
        register: &|_| {},
        pack_budget: &margin_service::retrieval::pack_budget,
    };
    let store = EvalStore::memory().await.unwrap();
    let (run, r, violations) = run_access(&env, "baseline", &store).await.unwrap();
    eprintln!(
        "access: {} principals x {} queries, {} leaks, {} by-id leaks of {} forbidden pairs, \
         {} hits removed, owner ranks identical {}, rank consistent {} ({} searches), p95 \
         owner/unfiltered {:.2}x ({:.2}/{:.2} ms)",
        r.principals.len(),
        r.queries,
        r.leaks.len(),
        r.by_id_leaks.len(),
        r.by_id_checked,
        r.removed(),
        r.owner_rank_identical(),
        r.rank_consistent(),
        r.rank_checked,
        r.latency_ratio(),
        r.owner_p95_ms,
        r.unfiltered_p95_ms
    );
    assert!(
        violations.is_empty(),
        "the access gates failed:\n{}",
        violations.join("\n")
    );
    assert!(run.valid);
    // The `[retrieval]` baseline doesn't move: the reader isn't a pipeline stage.
    let (cfg, _) = Paths::same(&evals).variant("baseline").unwrap();
    assert_eq!(run.pipeline_hash, cfg.hash());
    assert_eq!(
        cfg.hash(),
        margin_shared::pipeline::PipelineConfig::default().hash()
    );
}
