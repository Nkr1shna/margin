//! Retrieval on the embedded engine (`kv-mem`, the real HNSW and FULLTEXT indexes),
//! with passages seeded directly through `Store`.

use super::*;
use crate::pipeline::{Fusion, StageDeps};
use margin_shared::pipeline::{PipelineConfig, StageCfg};

fn unit(i: usize) -> Vec<f32> {
    let mut v = vec![0f32; 768];
    v[i] = 1.0;
    v
}

fn near(i: usize, j: usize, w: f32) -> Vec<f32> {
    let mut v = unit(i);
    v[j] = w;
    let n = (1.0 + w * w).sqrt();
    v.iter_mut().for_each(|x| *x /= n);
    v
}

struct P<'a> {
    id: &'a str,
    doc: &'a str,
    scope: &'a [&'a str],
    modality: &'a str,
    ord: i64,
    page: Option<i64>,
    text: &'a str,
    v: Vec<f32>,
}

fn row(p: P) -> PassageRow {
    PassageRow {
        id: p.id.into(),
        project_id: DEFAULT_PROJECT_ID.into(),
        document_id: p.doc.into(),
        origin: "source".into(),
        scope_ids: p.scope.iter().map(|s| s.to_string()).collect(),
        access: vec![ACCESS_COMPANY.into()],
        unit_id: match p.page {
            Some(n) => format!("{}:page:{n}", p.doc),
            None => format!("{}:{}", p.doc, p.id),
        },
        modality: p.modality.into(),
        ord: p.ord,
        page: p.page,
        start_line: Some(p.ord * 10 + 1),
        end_line: Some(p.ord * 10 + 9),
        start_ms: None,
        end_ms: None,
        section_path: format!("Part {}", p.ord / 2),
        text: p.text.into(),
        file: (p.modality != "text").then(|| format!("{}.png", p.id)),
        chunk_hash: format!("h-{}", p.id),
        embedder: "test".into(),
        vector: p.v,
    }
}

fn text(id: &str, doc: &str, scope: &[&str], ord: i64, text: &str, v: Vec<f32>) -> PassageRow {
    row(P {
        id,
        doc,
        scope,
        modality: "text",
        ord,
        page: None,
        text,
        v,
    })
}

/// Rare words only: BM25 scores a term found in most rows as 0.
async fn seeded() -> Store {
    let s = Store::memory().await.unwrap();
    s.insert_many(
        db::PASSAGE,
        &[
            text(
                "a",
                "d1",
                &["t1"],
                0,
                "The Carnot engine bounds thermal efficiency",
                unit(0),
            ),
            text(
                "b",
                "d1",
                &["t1"],
                1,
                "Entropy never decreases in an isolated system",
                unit(1),
            ),
            text(
                "c",
                "d2",
                &["t2"],
                0,
                "Photosynthesis stores light energy as sugar",
                unit(2),
            ),
            text(
                "e",
                "d3",
                &["c1"],
                0,
                "Fourier series expand periodic signals",
                near(3, 0, 0.3),
            ),
            text(
                "f",
                "d3",
                &["c1"],
                1,
                "Gibbs ringing overshoots near a jump",
                unit(4),
            ),
        ],
    )
    .await
    .unwrap();
    s
}

fn query(text: &str, vector: Vec<f32>, scope: Scope) -> Query {
    Query {
        text: text.into(),
        vector,
        scope,
        access: None,
        modalities: Vec::new(),
        k: 24,
        budget_tokens: 10_000,
        images_max: 4,
        diversity: Diversity::default(),
    }
}

fn retriever(s: &Store, name: &str) -> Arc<dyn Retriever> {
    let reg = Registry::standard(StageDeps { store: s.clone() });
    let cfg = PipelineConfig {
        retriever: StageCfg::named(name),
        ..PipelineConfig::default()
    };
    crate::pipeline::Pipeline::build(&cfg, &reg)
        .unwrap()
        .retriever
}

async fn put_topic(s: &Store, id: &str, parent: Option<&str>) {
    let now = Utc::now();
    s.put(
        db::TOPIC,
        id,
        &Topic {
            id: id.into(),
            parent_id: parent.map(str::to_string),
            title: id.into(),
            position: 0,
            created_at: now,
            updated_at: now,
        },
    )
    .await
    .unwrap();
}

async fn put_source(s: &Store, id: &str, topic: Option<&str>, workspace: Option<&str>) {
    s.put(
        db::SOURCE,
        id,
        &Source {
            id: id.into(),
            topic_id: topic.map(str::to_string),
            name: format!("{id}.md"),
            folder: None,
            kind: SourceKind::Text,
            bytes: 1,
            status: SourceStatus::Ready {
                chunks: 1,
                pages: None,
                warning: None,
            },
            created_at: Utc::now(),
            media_url: None,
            workspace_id: workspace.map(str::to_string),
            modified: None,
        },
    )
    .await
    .unwrap();
}

async fn put_chat(s: &Store, id: &str, sources: &[&str], workspaces: &[&str]) -> Chat {
    let chat: Chat = serde_json::from_value(json!({
        "id": id, "title": "c", "state": {"state": "idle"},
        "source_ids": sources, "workspace_ids": workspaces,
        "created_at": Utc::now(), "updated_at": Utc::now(),
    }))
    .unwrap();
    s.put(db::CHAT, id, &chat).await.unwrap();
    chat
}

fn ids(hits: &[Hit]) -> Vec<&str> {
    hits.iter().map(|h| h.passage.id.as_str()).collect()
}

// -- the index legs ------------------------------------------------------------

#[tokio::test]
async fn hybrid_rrf_fuses_both_legs_inside_the_scope() {
    let s = seeded().await;
    put_topic(&s, "t1", None).await;
    put_topic(&s, "t2", None).await;
    let r = retriever(&s, "hybrid-rrf");
    assert_eq!(r.name(), "hybrid-rrf");
    // "entropy" is lexical only (b); the vector is a's: both legs count.
    let hits = r
        .search(&query("entropy", unit(0), Scope::Topic { id: "t1".into() }))
        .await
        .unwrap();
    assert_eq!(ids(&hits).len(), 2);
    let a = hits.iter().find(|h| h.passage.id == "a").unwrap();
    let b = hits.iter().find(|h| h.passage.id == "b").unwrap();
    assert_eq!(a.dense_rank, Some(0));
    assert_eq!(b.lex_rank, Some(0));
    assert_eq!(
        b.highlight.as_deref(),
        Some("**Entropy** never decreases in an isolated system")
    );
    assert!(hits.iter().all(|h| h.score > 0.0));
    // The full row comes back, vector included.
    assert_eq!(a.passage.vector.len(), 768);
    assert_eq!(a.passage.section_path, "Part 0");
    // Nothing outside the scope, even when it matches both legs best.
    let hits = r
        .search(&query(
            "photosynthesis",
            unit(2),
            Scope::Topic { id: "t1".into() },
        ))
        .await
        .unwrap();
    assert!(!ids(&hits).contains(&"c"), "{:?}", ids(&hits));
}

#[tokio::test]
async fn hybrid_linear_weighs_both_legs_inside_the_scope() {
    let s = seeded().await;
    put_topic(&s, "t1", None).await;
    put_topic(&s, "t2", None).await;
    let r = retriever(&s, "hybrid-linear");
    assert_eq!(r.name(), "hybrid-linear");
    let hits = r
        .search(&query("entropy", unit(0), Scope::Topic { id: "t1".into() }))
        .await
        .unwrap();
    assert_eq!(ids(&hits).len(), 2);
    let a = hits.iter().find(|h| h.passage.id == "a").unwrap();
    let b = hits.iter().find(|h| h.passage.id == "b").unwrap();
    assert_eq!(a.dense_rank, Some(0));
    assert_eq!(b.lex_rank, Some(0));
    assert!(b.highlight.is_some());
    // Each leg's best scores 1 times its weight (both 1 by default).
    assert!(hits[0].score >= 1.0 - 1e-4);
    assert!(hits.windows(2).all(|w| w[0].score >= w[1].score));
    // The weights come from the retriever's params.
    let reg = Registry::standard(StageDeps { store: s.clone() });
    let mut cfg = PipelineConfig {
        retriever: StageCfg::named("hybrid-linear"),
        ..PipelineConfig::default()
    };
    cfg.retriever
        .params
        .insert("lex_weight".into(), serde_json::json!(0.0));
    let r = crate::pipeline::Pipeline::build(&cfg, &reg)
        .unwrap()
        .retriever;
    let hits = r
        .search(&query("entropy", unit(0), Scope::Topic { id: "t1".into() }))
        .await
        .unwrap();
    assert_eq!(hits[0].passage.id, "a", "the dense leg alone decides");
}

#[tokio::test]
async fn dense_and_bm25_run_one_leg_each() {
    let s = seeded().await;
    let lib = Scope::Library;
    let hits = retriever(&s, "dense")
        .search(&query("photosynthesis", unit(4), lib.clone()))
        .await
        .unwrap();
    assert_eq!(hits[0].passage.id, "f");
    assert!(hits.iter().all(|h| h.lex_rank.is_none()));
    assert!((hits[0].score - 1.0).abs() < 1e-4);
    let hits = retriever(&s, "bm25")
        .search(&query("photosynthesis", unit(4), lib.clone()))
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["c"]);
    assert_eq!(hits[0].lex_rank, Some(0));
    // Without a vector (no embedding model) the hybrid searches by words alone.
    let hits = retriever(&s, "hybrid-rrf")
        .search(&query("gibbs", Vec::new(), lib.clone()))
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["f"]);
    // And with no words, by meaning alone.
    let hits = retriever(&s, "hybrid-rrf")
        .search(&query("  ", unit(2), lib))
        .await
        .unwrap();
    assert_eq!(hits[0].passage.id, "c");
}

#[tokio::test]
async fn filters_are_inside_the_index_scans() {
    let s = seeded().await;
    let r = IndexRetriever::new(s.clone(), Legs::Hybrid, &Default::default());
    let filter = Filter {
        resolved: Resolved {
            keys: vec!["t1".into(), "d3".into()],
            ..Default::default()
        },
        modalities: vec!["text".into()],
        access: Some(vec!["company".into()]),
    };
    let explain = |sql: String, extra: (&'static str, Value)| {
        let s = s.clone();
        let mut binds = filter.binds();
        binds.push(extra);
        async move {
            let plan: Vec<Value> = s
                .query_rows(&format!("{sql} EXPLAIN"), binds)
                .await
                .unwrap();
            serde_json::to_string(&plan).unwrap()
        }
    };
    let plan = explain(r.dense_sql(&filter, 40, "id"), ("q", json!(unit(0)))).await;
    assert!(
        plan.contains("KnnScan") && plan.contains("passage_vector"),
        "{plan}"
    );
    // The scope, modality and access conditions are the KNN scan's own predicate.
    let knn = plan.split("KnnScan").next().unwrap();
    assert!(
        knn.contains("scope_ids CONTAINSANY") && knn.contains("CONTAINSANY ['company']"),
        "{plan}"
    );
    let plan = explain(r.lex_sql(&filter, 40, "id"), ("text", json!("entropy"))).await;
    assert!(
        plan.contains("FullTextScan") && plan.contains("passage_text"),
        "{plan}"
    );
    // No passage query of this module scans the table by `list_where`.
    let plan = explain(
        "SELECT id FROM passage WHERE document_id = $d".into(),
        ("d", json!("d1")),
    )
    .await;
    assert!(plan.contains("passage_document"), "{plan}");
}

#[tokio::test]
async fn access_is_a_prefilter() {
    let s = seeded().await;
    let mut secret = text("z", "d9", &["t1"], 0, "Quarterly entropy forecast", unit(1));
    secret.access = vec!["company/finance".into()];
    s.insert_many(db::PASSAGE, &[secret]).await.unwrap();
    // Passages carry their document's paths; the reader's visible paths come from the
    // documents.
    let doc: Document = serde_json::from_value(json!({
        "id": "d9", "project_id": DEFAULT_PROJECT_ID, "origin": {"origin": "agent", "name": "x"},
        "name": "forecast", "kind": "text", "status": {"state": "ready", "passages": 1},
        "generation": 1, "bytes": 1, "access": ["company/finance"],
        "created_at": Utc::now(), "updated_at": Utc::now(),
    }))
    .unwrap();
    s.put(db::DOCUMENT, "d9", &doc).await.unwrap();
    put_topic(&s, "t1", None).await;
    let r = retriever(&s, "hybrid-rrf");
    let mut q = query("entropy", unit(1), Scope::Topic { id: "t1".into() });
    q.access = Some(vec!["company/engineering".into()]);
    let hits = r.search(&q).await.unwrap();
    assert!(!ids(&hits).contains(&"z"));
    // Engineering doesn't see company-wide passages either: paths are prefixes down.
    assert!(hits.is_empty(), "{:?}", ids(&hits));
    q.access = Some(vec!["company".into()]);
    let hits = r.search(&q).await.unwrap();
    assert!(ids(&hits).contains(&"z") && ids(&hits).contains(&"b"));
    q.access = Some(vec!["company/finance".into()]);
    assert_eq!(ids(&r.search(&q).await.unwrap()), ["z"]);
    assert!(may_see(&["company".into()], "company/finance/payroll"));
    assert!(!may_see(&["company/fin".into()], "company/finance"));
    assert!(!may_see(&["company/finance".into()], "company"));
}

#[tokio::test]
async fn modalities_filter_the_legs() {
    let s = seeded().await;
    s.insert_many(
        db::PASSAGE,
        &[row(P {
            id: "pic",
            doc: "d1",
            scope: &["t1"],
            modality: "picture",
            ord: 5,
            page: Some(1),
            text: "entropy diagram",
            v: unit(1),
        })],
    )
    .await
    .unwrap();
    let mut q = query("entropy", unit(1), Scope::Library);
    q.modalities = vec![Modality::Picture];
    assert_eq!(
        ids(&retriever(&s, "hybrid-rrf").search(&q).await.unwrap()),
        ["pic"]
    );
    q.modalities = vec![Modality::Text];
    assert!(!ids(&retriever(&s, "hybrid-rrf").search(&q).await.unwrap()).contains(&"pic"));
}

// -- scope resolution ------------------------------------------------------------

#[tokio::test]
async fn scopes_resolve_to_keys_and_unknown_ids_are_bad() {
    let s = Store::memory().await.unwrap();
    put_topic(&s, "subject", None).await;
    put_topic(&s, "topic", Some("subject")).await;
    put_source(&s, "s1", Some("topic"), None).await;
    put_source(&s, "s2", None, Some("w1")).await;
    s.put(
        db::WORKSPACE,
        "w1",
        &json!({"id": "w1", "name": "notes", "device_id": "dev", "device_name": "Mac",
                "platform": "macos", "location": "~/notes", "created_at": Utc::now()}),
    )
    .await
    .unwrap();
    let doc = Document {
        id: "doc-s1".into(),
        project_id: DEFAULT_PROJECT_ID.into(),
        origin: Origin::Source {
            source_id: "s1".into(),
        },
        name: "s1.md".into(),
        kind: SourceKind::Text,
        status: margin_shared::context::DocumentStatus::Ready { passages: 1 },
        generation: 1,
        bytes: 1,
        actor: "owner".into(),
        access: vec!["company".into()],
        created_at: Utc::now(),
        updated_at: Utc::now(),
    };
    s.put(db::DOCUMENT, &doc.id, &doc).await.unwrap();
    put_chat(&s, "c1", &["s1"], &["w1"]).await;

    let r = resolve(
        &s,
        &Scope::Topic {
            id: "subject".into(),
        },
    )
    .await
    .unwrap();
    assert_eq!(r.keys, ["subject", "topic", "s1", "doc-s1"]);
    assert_eq!(r.source_ids, ["s1"]);
    let r = resolve(&s, &Scope::Chat { id: "c1".into() }).await.unwrap();
    assert_eq!(r.keys, ["c1", "s1", "doc-s1", "w1", "s2"]);
    let r = resolve(
        &s,
        &Scope::Sources {
            ids: vec!["doc-s1".into()],
        },
    )
    .await
    .unwrap();
    assert_eq!(r.source_ids, ["s1"]);
    let r = resolve(&s, &Scope::Library).await.unwrap();
    assert!(r.library && r.source_ids.len() == 2);
    for bad in [
        Scope::Topic { id: "nope".into() },
        Scope::Chat { id: "nope".into() },
        Scope::Workspace { id: "nope".into() },
        Scope::Union {
            scopes: vec![
                Scope::Library,
                Scope::Sources {
                    ids: vec!["nope".into()],
                },
            ],
        },
    ] {
        let err = resolve(&s, &bad).await.unwrap_err();
        assert!(err.downcast_ref::<BadScope>().is_some(), "{err}");
    }
    // The chat's default scope: its files and folders, then its files' topics.
    let chat: Chat = s.get(db::CHAT, "c1").await.unwrap().unwrap();
    assert_eq!(
        chat_scope(&s, &chat).await.unwrap(),
        Scope::Union {
            scopes: vec![
                Scope::Chat { id: "c1".into() },
                Scope::Topic { id: "topic".into() }
            ]
        }
    );
    // One project, created once.
    let p = project(&s).await.unwrap();
    assert_eq!(
        (p.id.as_str(), p.name.as_str()),
        (DEFAULT_PROJECT_ID, "My library")
    );
    assert_eq!(project(&s).await.unwrap().id, p.id);
}

// -- fusion -------------------------------------------------------------------------

fn hit(id: &str, doc: &str, ord: i64, score: f32, v: Vec<f32>) -> Hit {
    Hit {
        passage: text(id, doc, &[], ord, &format!("passage {id} words"), v),
        score,
        dense_rank: None,
        lex_rank: None,
        rerank: None,
        highlight: None,
    }
}

fn fcfg(k: usize) -> FusionCfg {
    FusionCfg {
        k_fused: k,
        rrf_k: 60,
        params: Default::default(),
    }
}

#[test]
fn rrf_across_query_strings() {
    let a = vec![
        hit("x", "d", 0, 0.9, unit(0)),
        hit("y", "d", 1, 0.5, unit(1)),
    ];
    let mut y2 = hit("y", "d", 1, 0.4, unit(1));
    y2.highlight = Some("**y**".into());
    y2.lex_rank = Some(0);
    let b = vec![y2, hit("z", "d", 2, 0.3, unit(2))];
    let fused = Rrf.fuse(vec![a.clone(), b], &fcfg(10));
    assert_eq!(ids(&fused), ["y", "x", "z"]);
    assert!((fused[0].score - (1.0 / 62.0 + 1.0 / 61.0)).abs() < 1e-6);
    assert_eq!(fused[0].highlight.as_deref(), Some("**y**"));
    assert_eq!(fused[0].lex_rank, Some(0));
    // One list passes as it is, cut to k.
    let one = Rrf.fuse(vec![a.clone(), Vec::new()], &fcfg(1));
    assert_eq!(ids(&one), ["x"]);
    assert_eq!(one[0].score, 0.9);
    // Linear: normalised scores summed with weights.
    let lin = Linear.fuse(
        vec![
            a,
            vec![
                hit("y", "d", 1, 2.0, unit(1)),
                hit("x", "d", 0, 1.0, unit(0)),
            ],
        ],
        &FusionCfg {
            params: json!({"weights": [1.0, 3.0]}).as_object().unwrap().clone(),
            ..fcfg(10)
        },
    );
    assert_eq!(ids(&lin), ["y", "x"]);
    assert!((lin[0].score - 3.0).abs() < 1e-6);
}

// -- packing ------------------------------------------------------------------------

fn budget(tokens: usize, k: usize) -> PackBudget {
    PackBudget {
        tokens,
        images_max: 2,
        min_score_pct: 0,
        passages_min: 0,
        k,
        diversity: Diversity::default(),
    }
}

#[test]
fn the_budget_is_a_share_of_the_window() {
    let b = Budget::default();
    let pb = pack_budget(&b, 200_000, 12);
    assert_eq!(
        (pb.tokens, pb.images_max, pb.k, pb.min_score_pct),
        (100_000, 8, 12, 25)
    );
    let b = Budget {
        share_of_window: 0.1,
        images_max: 1,
        passages_min: 2,
        min_score_pct: 50,
    };
    let pb = pack_budget(&b, 1_000_000, 4);
    assert_eq!((pb.tokens, pb.images_max, pb.passages_min), (100_000, 1, 2));
    assert_eq!(context_window("claude-sonnet-5-5"), 200_000);
    assert_eq!(context_window("gpt-5.2"), 400_000);
    assert_eq!(context_window("test-model"), DEFAULT_CONTEXT_WINDOW);
}

#[test]
fn the_packer_never_exceeds_the_budget_and_keeps_the_best() {
    let p = MmrV1::default();
    let hits: Vec<Hit> = (0..10)
        .map(|i| {
            hit(
                &format!("h{i}"),
                &format!("d{i}"),
                0,
                1.0 - i as f32 * 0.05,
                unit(i),
            )
        })
        .collect();
    let each = estimate_tokens(&hits[0].passage.text);
    let packed = p.pack(hits.clone(), &budget(each * 3 + 1, 12));
    assert_eq!(packed.len(), 3);
    assert!(packed.iter().map(|x| x.tokens).sum::<usize>() <= each * 3 + 1);
    assert_eq!(packed[0].hits[0].passage.id, "h0");
    // k bounds it whatever the budget.
    assert_eq!(p.pack(hits.clone(), &budget(1_000_000, 4)).len(), 4);
    // The score threshold drops the weak tail before the budget.
    let mut b = budget(1_000_000, 12);
    b.min_score_pct = 50;
    let packed = p.pack(hits.clone(), &b);
    assert_eq!(
        packed.len(),
        6,
        "{:?}",
        packed.iter().map(|x| &x.text).collect::<Vec<_>>()
    );
    // …unless passages_min asks for more.
    b.passages_min = 8;
    assert_eq!(p.pack(hits, &b).len(), 8);
}

#[test]
fn the_packer_caps_documents_and_spreads_picks() {
    let p = MmrV1::default();
    // Six near-identical passages of one document, two others.
    let mut hits: Vec<Hit> = (0..6)
        .map(|i| {
            hit(
                &format!("a{i}"),
                "da",
                i * 3,
                1.0 - i as f32 * 0.01,
                near(0, 1, 0.01 * i as f32),
            )
        })
        .collect();
    hits.push(hit("b", "db", 0, 0.8, unit(5)));
    hits.push(hit("c", "dc", 0, 0.7, unit(6)));
    let packed = p.pack(hits, &budget(1_000_000, 4));
    let docs: Vec<&str> = packed
        .iter()
        .map(|x| x.hits[0].passage.document_id.as_str())
        .collect();
    assert_eq!(docs.iter().filter(|d| **d == "da").count(), 2, "{docs:?}");
    assert!(docs.contains(&"db") && docs.contains(&"dc"));
}

#[test]
fn the_packer_dedups_units_and_merges_neighbours() {
    let p = MmrV1::default();
    let page_text = row(P {
        id: "t",
        doc: "pdf",
        scope: &[],
        modality: "text",
        ord: 0,
        page: Some(3),
        text: "Page three explains the Gibbs phenomenon.",
        v: unit(0),
    });
    let page_pic = row(P {
        id: "pic",
        doc: "pdf",
        scope: &[],
        modality: "picture",
        ord: 1,
        page: Some(3),
        text: "Page three explains the Gibbs phenomenon.",
        v: unit(1),
    });
    let mk = |passage: PassageRow, score: f32| Hit {
        passage,
        score,
        dense_rank: None,
        lex_rank: None,
        rerank: None,
        highlight: None,
    };
    // The text outranks its page's picture: the picture goes.
    let packed = p.pack(
        vec![mk(page_text.clone(), 0.9), mk(page_pic.clone(), 0.5)],
        &budget(1_000_000, 8),
    );
    assert_eq!(packed.len(), 1);
    assert_eq!(packed[0].citation.image_url, None);
    // The picture wins outright: it goes with the text, counted as a picture.
    let packed = p.pack(
        vec![mk(page_pic.clone(), 0.9), mk(page_text.clone(), 0.5)],
        &budget(1_000_000, 8),
    );
    assert_eq!(packed.len(), 1);
    assert_eq!(packed[0].citation.image_url.as_deref(), Some("pic.png"));
    assert_eq!(packed[0].hits.len(), 2);
    assert!(packed[0].tokens >= PICTURE_TOKENS);
    // No room for pictures: the text alone.
    let mut b = budget(1_000_000, 8);
    b.images_max = 0;
    let packed = p.pack(vec![mk(page_pic.clone(), 0.9), mk(page_text, 0.5)], &b);
    assert_eq!(packed[0].citation.image_url, None);
    assert!(packed[0].tokens < PICTURE_TOKENS);
    // A picture alone counts against images_max.
    let packed = p.pack(vec![mk(page_pic, 0.9)], &b);
    assert!(packed.is_empty());

    // Adjacent chunks of one document become one passage, the overlap said once, with
    // the first chunk's locator.
    let first = text(
        "o1",
        "doc",
        &[],
        4,
        "Alpha beta. The shared sentence here.",
        unit(0),
    );
    let second = text(
        "o2",
        "doc",
        &[],
        5,
        "The shared sentence here. Gamma delta.",
        unit(1),
    );
    let other = text("o3", "doc", &[], 9, "Far away chunk.", unit(2));
    let packed = p.pack(
        vec![mk(second, 0.9), mk(other, 0.8), mk(first, 0.7)],
        &budget(1_000_000, 8),
    );
    assert_eq!(packed.len(), 2);
    assert_eq!(
        packed[0].text,
        "Alpha beta. The shared sentence here.\nGamma delta."
    );
    assert_eq!(packed[0].hits[0].passage.id, "o2", "hits are best first");
    assert_eq!(packed[0].citation.start_line, Some(41));
    assert_eq!(packed[0].citation.document_id.as_deref(), Some("doc"));
    assert_eq!(
        packed[0].citation.excerpt,
        "Alpha beta. The shared sentence here."
    );
}

#[test]
fn the_packer_says_a_stretch_of_a_recording_once() {
    let p = MmrV1::default();
    let spoken = |id: &str, from: i64, to: i64, score: f32, modality: &str| {
        let mut r = text(
            id,
            "talk",
            &[],
            from / 1000,
            "words said here",
            unit((from / 1000) as usize),
        );
        r.modality = modality.into();
        r.start_ms = Some(from);
        r.end_ms = Some(to);
        Hit {
            passage: r,
            score,
            dense_rank: None,
            lex_rank: None,
            rerank: None,
            highlight: None,
        }
    };
    let packed = p.pack(
        vec![
            spoken("transcript", 0, 24_000, 0.9, "text"),
            spoken("w0", 0, 10_000, 0.8, "sound"),
            spoken("w1", 10_000, 20_000, 0.7, "sound"),
            spoken("other", 60_000, 70_000, 0.6, "sound"),
        ],
        &budget(1_000_000, 8),
    );
    let ids: Vec<&str> = packed
        .iter()
        .map(|x| x.hits[0].passage.id.as_str())
        .collect();
    assert_eq!(ids, ["transcript", "other"]);
}

// -- the cache ------------------------------------------------------------------------

#[test]
fn the_cache_is_an_lru_invalidated_by_document() {
    let c = RetrievalCache::new(2);
    let packed = |doc: &str| {
        Arc::new(vec![Packed {
            hits: vec![hit("x", doc, 0, 1.0, unit(0))],
            text: "x".into(),
            tokens: 1,
            citation: hit_citation(&hit("x", doc, 0, 1.0, unit(0))),
        }])
    };
    c.insert("a".into(), packed("d1"));
    c.insert("b".into(), packed("d2"));
    assert!(c.get("a").is_some());
    c.insert("c".into(), packed("d3"));
    assert!(c.get("b").is_none(), "the least recently used goes");
    assert_eq!(c.len(), 2);
    c.invalidate("d1");
    assert!(c.get("a").is_none());
    assert!(c.get("c").is_some());
}

#[test]
fn query_plans() {
    let p = QueryPlan::lesson(
        "Fourier",
        "Series of sines",
        &["Why sines?".into()],
        Some("teach me"),
    );
    assert_eq!(
        p.queries,
        [
            "Fourier Series of sines",
            "Series of sines",
            "Why sines?",
            "teach me"
        ]
    );
    let p = QueryPlan::lesson("A", "", &[], None);
    assert_eq!(p.queries, ["A"]);
    assert_eq!(
        QueryPlan::from_strings(["x".into(), " X ".into()]).queries,
        ["x"]
    );
}

// -- reading a document ------------------------------------------------------------

#[tokio::test]
async fn documents_read_by_ord_page_section_and_picture() {
    let s = seeded().await;
    s.insert_many(
        db::PASSAGE,
        &[
            text("b2", "d1", &[], 2, "third", unit(5)),
            text("b3", "d1", &[], 3, "fourth", unit(6)),
            row(P {
                id: "pp",
                doc: "d1",
                scope: &[],
                modality: "picture",
                ord: 9,
                page: Some(2),
                text: "a figure",
                v: unit(7),
            }),
        ],
    )
    .await
    .unwrap();
    let rows = passages_by_ord(&s, "d1", 1, 10, 2).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.ord).collect::<Vec<_>>(), [1, 2]);
    let outline = document_outline(&s, "d1").await.unwrap();
    assert_eq!(outline.len(), 2);
    assert_eq!(
        (
            outline[0].section_path.as_str(),
            outline[0].from_ord,
            outline[0].to_ord
        ),
        ("Part 0", 0, 1)
    );
    assert_eq!(
        (outline[1].start_line, outline[1].end_line),
        (Some(21), Some(39))
    );
    let pic = document_picture(&s, "d1", Some(2)).await.unwrap().unwrap();
    assert_eq!(pic.file.as_deref(), Some("pp.png"));
    assert!(document_picture(&s, "d1", Some(7)).await.unwrap().is_none());
    assert!(matches!(
        doc_state(&s, "d1").await.unwrap(),
        DocState::Ready
    ));
    assert!(matches!(
        doc_state(&s, "nope").await.unwrap(),
        DocState::Unknown
    ));
    let h = context_hit(&pic, 0.5, None);
    assert_eq!((h.modality, h.page), (Modality::Picture, Some(2)));
}

#[tokio::test]
async fn the_index_query_finds_scope_ids_without_records_behind_them() {
    // The eval index files passages under a topic and a folder id alone.
    let s = seeded().await;
    let r = retriever(&s, "hybrid-rrf");
    let hits = r
        .search(&query("entropy", unit(1), Scope::Topic { id: "t1".into() }))
        .await
        .unwrap();
    assert!(ids(&hits).contains(&"b"), "{:?}", ids(&hits));
    assert!(!ids(&hits).contains(&"c"));
    let hits = r
        .search(&query(
            "photosynthesis",
            unit(2),
            Scope::Sources {
                ids: vec!["d2".into()],
            },
        ))
        .await
        .unwrap();
    assert_eq!(ids(&hits), ["c"]);
    // The checked resolution still refuses an unknown topic.
    let err = resolve(&s, &Scope::Topic { id: "t1".into() })
        .await
        .unwrap_err();
    assert!(err.downcast_ref::<BadScope>().is_some());
}

#[test]
fn text_of_another_page_is_not_merged_in() {
    let p = MmrV1::default();
    let mk = |id, ord, page, score| Hit {
        passage: row(P {
            id,
            doc: "pdf",
            scope: &[],
            modality: "text",
            ord,
            page: Some(page),
            text: id,
            v: unit(ord as usize),
        }),
        score,
        dense_rank: None,
        lex_rank: None,
        rerank: None,
        highlight: None,
    };
    let packed = p.pack(
        vec![mk("one", 0, 1, 0.9), mk("two", 1, 2, 0.8)],
        &budget(1_000_000, 8),
    );
    let pages: Vec<Option<u32>> = packed.iter().map(|p| p.citation.page).collect();
    assert_eq!(pages, [Some(1), Some(2)]);
}
