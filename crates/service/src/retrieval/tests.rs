//! Retrieval on the embedded engine (`kv-mem`, the real HNSW and FULLTEXT indexes),
//! with passages seeded directly through `Store`.

use super::*;
use crate::pipeline::{Fusion, StageDeps};
use margin_shared::pipeline::{PipelineConfig, StageCfg};

/// A reader of `company` (every seeded row's audience) with no denials.
fn company_reader() -> Reader {
    Reader::sorted(vec![ACCESS_COMPANY.into()], vec![ACCESS_COMPANY.into()])
}

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
        deny: Vec::new(),
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
        embedder: String::new(),
        vector,
        scope,
        reader: company_reader(),
        modalities: Vec::new(),
        k: 24,
        budget_tokens: 10_000,
        images_max: 4,
        diversity: Diversity::default(),
    }
}

fn retriever(s: &Store, name: &str) -> Arc<dyn Retriever> {
    let reg = Registry::standard(StageDeps::new(s.clone()));
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
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
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
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
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
    let reg = Registry::standard(StageDeps::new(s.clone()));
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
        reader: Reader::sorted(vec!["company".into()], vec!["person/p".into()]),
        embedder: "test".into(),
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
    // The scope, modality, embedder and the reader's two conditions are the KNN scan's
    // own predicate (L2 D1).
    let knn = plan.split("KnnScan").next().unwrap();
    assert!(
        knn.contains("scope_ids CONTAINSANY") && knn.contains("CONTAINSANY ['company']"),
        "{plan}"
    );
    assert!(knn.contains("CONTAINSNONE ['person/p']"), "{plan}");
    assert!(knn.contains("embedder = 'test'"), "{plan}");
    // The lexical leg doesn't filter by embedder.
    assert!(!r.lex_sql(&filter, 40, "id").contains("embedder"));
    let plan = explain(r.lex_sql(&filter, 40, "id"), ("text", json!("entropy"))).await;
    assert!(
        plan.contains("FullTextScan") && plan.contains("passage_text"),
        "{plan}"
    );
    assert!(
        plan.contains("CONTAINSANY ['company']") && plan.contains("CONTAINSNONE ['person/p']"),
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

/// L2 D1: the reader's paths are matched exactly (the hierarchy is in the reader's
/// expansion, never a prefix rule here) and its denials are refused, both inside the
/// scan.
#[tokio::test]
async fn access_is_exact_membership_with_denials() {
    let s = seeded().await;
    let mut secret = text("z", "d9", &["t1"], 0, "Quarterly entropy forecast", unit(1));
    secret.access = vec!["company/finance".into()];
    let mut denied = text("y", "d8", &["t1"], 0, "Entropy of the ledger", unit(1));
    denied.deny = vec!["person/bob".into()];
    s.insert_many(db::PASSAGE, &[secret, denied]).await.unwrap();
    put_topic(&s, "t1", None).await;
    let r = retriever(&s, "hybrid-rrf");
    let mut q = query("entropy", unit(1), Scope::Topic { id: "t1".into() });
    // `company/engineering` alone reads nothing `company` holds.
    q.reader = Reader::sorted(vec!["company/engineering".into()], Vec::new());
    assert!(r.search(&q).await.unwrap().is_empty());
    // `company` does not read `company/finance`: no prefix rule.
    q.reader = company_reader();
    let hits = r.search(&q).await.unwrap();
    assert!(ids(&hits).contains(&"b") && ids(&hits).contains(&"y"));
    assert!(!ids(&hits).contains(&"z"), "{:?}", ids(&hits));
    // A member of finance holds `company` and `company/finance` (the expansion).
    q.reader = Reader::sorted(
        vec![
            "company".into(),
            "company/finance".into(),
            "person/f".into(),
        ],
        vec![
            "company".into(),
            "company/finance".into(),
            "person/f".into(),
        ],
    );
    let hits = r.search(&q).await.unwrap();
    assert!(ids(&hits).contains(&"z") && ids(&hits).contains(&"b"));
    // Bob is denied y by name.
    q.reader = Reader::sorted(
        vec!["company".into(), "person/bob".into()],
        vec!["company".into(), "person/bob".into()],
    );
    let hits = r.search(&q).await.unwrap();
    assert!(ids(&hits).contains(&"b") && !ids(&hits).contains(&"y"));
    // Every leg, the same rule.
    for name in ["dense", "bm25", "hybrid-linear"] {
        let hits = retriever(&s, name).search(&q).await.unwrap();
        assert!(
            !ids(&hits).contains(&"y") && !ids(&hits).contains(&"z"),
            "{name}"
        );
    }
    // A reader with no paths reads nothing, without running a query.
    q.reader = Reader::sorted(Vec::new(), Vec::new());
    assert!(r.search(&q).await.unwrap().is_empty());
    // The filter's SQL carries both conjuncts always, bound from the reader.
    let filter = Filter::new(&s, &q).await.unwrap();
    assert!(filter.sql().ends_with(Reader::sql()));
    let binds = filter.binds();
    assert!(binds.iter().any(|(k, _)| *k == "paths") && binds.iter().any(|(k, _)| *k == "deny"));
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
        owner: None,
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
        deny: Vec::new(),
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
    // D13: the answer's output is kept out of the share.
    let pb = pack_budget_reserving(&Budget::default(), 200_000, 32_000, 12);
    assert_eq!(pb.tokens, 68_000);
    assert_eq!(
        pack_budget_reserving(&Budget::default(), 1_000, 32_000, 4).tokens,
        0
    );
    let w = margin_ai::ModelWindow {
        input: 1_000_000,
        output: 128_000,
    };
    assert_eq!(output_reserve(Provider::Claude, Route::Chat, w), 32_000);
    assert_eq!(output_reserve(Provider::Claude, Route::Write, w), 64_000);
    assert_eq!(output_reserve(Provider::OpenAi, Route::Write, w), 128_000);
    let small = margin_ai::ModelWindow {
        input: 200_000,
        output: 8_000,
    };
    assert_eq!(output_reserve(Provider::Claude, Route::Write, small), 8_000);
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
    let reader = company_reader();
    let rows = passages_by_ord(&s, &reader, "d1", 1, 10, 2).await.unwrap();
    assert_eq!(rows.iter().map(|r| r.ord).collect::<Vec<_>>(), [1, 2]);
    let outline = document_outline(&s, &reader, "d1").await.unwrap();
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
    let pic = document_picture(&s, &reader, "d1", Some(2))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(pic.file.as_deref(), Some("pp.png"));
    assert!(
        document_picture(&s, &reader, "d1", Some(7))
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        doc_state(&s, &reader, "d1").await.unwrap(),
        DocState::Ready
    ));
    // A reader who can't read d1 finds nothing by id: no rows, no outline, Unknown.
    let other = Reader::sorted(vec!["company/sales".into()], Vec::new());
    assert!(
        passages_by_ord(&s, &other, "d1", 0, 10, 10)
            .await
            .unwrap()
            .is_empty()
    );
    assert!(document_outline(&s, &other, "d1").await.unwrap().is_empty());
    assert!(
        document_picture(&s, &other, "d1", Some(2))
            .await
            .unwrap()
            .is_none()
    );
    assert!(matches!(
        doc_state(&s, &other, "d1").await.unwrap(),
        DocState::Unknown
    ));
    assert!(matches!(
        doc_state(&s, &reader, "nope").await.unwrap(),
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

#[tokio::test]
async fn the_dense_leg_only_compares_vectors_of_the_query_embedder() {
    let s = seeded().await;
    let lib = Scope::Library;
    let mut q = query("photosynthesis", unit(4), lib);
    // Every seeded row is of embedder "test".
    q.embedder = "test".into();
    let hits = retriever(&s, "dense").search(&q).await.unwrap();
    assert_eq!(hits[0].passage.id, "f");
    // Vectors of another embedder (a half-finished re-embed) are never compared...
    q.embedder = "embeddinggemma-2-onnx-q8/title-prefix-v1".into();
    assert!(retriever(&s, "dense").search(&q).await.unwrap().is_empty());
    // ...while the lexical leg still finds them.
    let hits = retriever(&s, "hybrid-rrf").search(&q).await.unwrap();
    assert_eq!(ids(&hits), ["c"]);
    assert!(hits.iter().all(|h| h.dense_rank.is_none()));
}

#[test]
fn the_trace_counts_what_the_reranker_saw_and_dropped() {
    let fused = vec![
        hit("a", "d1", 0, 0.9, unit(0)),
        hit("b", "d1", 1, 0.5, unit(1)),
        hit("c", "d2", 0, 0.1, unit(2)),
    ];
    // "none", or a purpose passed through: nothing.
    assert_eq!(reranked_count(&fused, &fused), None);
    // Two graded, one dropped (grade 0).
    let mut out = vec![fused[1].clone(), fused[0].clone()];
    out[0].rerank = Some(3.0);
    out[1].rerank = Some(1.0);
    assert_eq!(reranked_count(&fused, &out), Some(3));
}

#[tokio::test]
async fn the_rewriter_sees_the_names_of_the_documents_in_scope() {
    let s = Store::memory().await.unwrap();
    for (id, name, access) in [
        ("d1", "b-notes.pdf", ACCESS_COMPANY),
        ("d2", "a-paper.pdf", "company/eng"),
        ("d3", "other.md", "person/p"),
    ] {
        let now = Utc::now();
        s.put(
            db::DOCUMENT,
            id,
            &Document {
                owner: None,
                id: id.into(),
                project_id: DEFAULT_PROJECT_ID.into(),
                origin: Origin::Agent { name: "t".into() },
                name: name.into(),
                kind: SourceKind::Text,
                status: margin_shared::context::DocumentStatus::Ready { passages: 1 },
                generation: 1,
                bytes: 1,
                actor: "owner".into(),
                access: vec![access.into()],
                deny: Vec::new(),
                created_at: now,
                updated_at: now,
            },
        )
        .await
        .unwrap();
    }
    let resolved = Resolved {
        keys: vec!["d1".into(), "d2".into()],
        ..Default::default()
    };
    let all = Reader::sorted(
        vec![
            ACCESS_COMPANY.into(),
            "company/eng".into(),
            "person/p".into(),
        ],
        Vec::new(),
    );
    assert_eq!(
        scope_names(&s, &resolved, &all).await,
        ["a-paper.pdf", "b-notes.pdf"]
    );
    let library = Resolved {
        library: true,
        ..Default::default()
    };
    assert_eq!(scope_names(&s, &library, &all).await.len(), 3);
    // A reader's access keeps the names of documents they can't read out of the
    // rewriter's prompt, by exact membership (L2): "company" reads neither company/eng
    // nor person/p.
    assert_eq!(
        scope_names(&s, &library, &company_reader()).await,
        ["b-notes.pdf"]
    );
    let eng = Reader::sorted(vec!["company/eng".into()], Vec::new());
    assert_eq!(scope_names(&s, &resolved, &eng).await, ["a-paper.pdf"]);
    let nobody = Reader::sorted(Vec::new(), Vec::new());
    assert!(scope_names(&s, &library, &nobody).await.is_empty());
}

// -- L2: readers on by-id reads, the cache key, the rewriter's names ------------------

fn doc_row(id: &str, owner: Option<&str>, access: &[&str], deny: &[&str]) -> Document {
    let now = Utc::now();
    Document {
        owner: owner.map(str::to_string),
        id: id.into(),
        project_id: DEFAULT_PROJECT_ID.into(),
        origin: Origin::Agent { name: "t".into() },
        name: format!("{id}.md"),
        kind: SourceKind::Text,
        status: margin_shared::context::DocumentStatus::Ready { passages: 1 },
        generation: 1,
        bytes: 1,
        actor: "owner".into(),
        access: access.iter().map(|a| a.to_string()).collect(),
        deny: deny.iter().map(|d| d.to_string()).collect(),
        created_at: now,
        updated_at: now,
    }
}

fn person_reader(person: &str, groups: &[&str]) -> Reader {
    let mut paths: Vec<String> = groups.iter().map(|g| g.to_string()).collect();
    paths.push(format!("person/{person}"));
    Reader::sorted(paths.clone(), paths)
}

/// Every by-id read answers nothing for a document the reader can't read (so routes
/// answer 404, never 403): by audience, and by a denial naming the reader.
#[tokio::test]
async fn every_by_id_read_refuses_an_unreadable_document() {
    let s = Store::memory().await.unwrap();
    let audience = |mut r: PassageRow| {
        r.access = vec!["company/sales".into()];
        r.deny = vec!["person/bob".into()];
        r
    };
    s.insert_many(
        db::PASSAGE,
        &[
            audience(row(P {
                id: "s0",
                doc: "sd",
                scope: &[],
                modality: "text",
                ord: 0,
                page: Some(1),
                text: "pipeline forecast",
                v: unit(0),
            })),
            audience(row(P {
                id: "s1",
                doc: "sd",
                scope: &[],
                modality: "picture",
                ord: 1,
                page: Some(1),
                text: "a chart",
                v: unit(1),
            })),
        ],
    )
    .await
    .unwrap();
    s.put(
        db::DOCUMENT,
        "sd",
        &doc_row("sd", Some("alice"), &["company/sales"], &["person/bob"]),
    )
    .await
    .unwrap();
    let sales = person_reader("sam", &["company", "company/sales"]);
    let bob = person_reader("bob", &["company", "company/sales"]);
    let engineer = person_reader("eve", &["company", "company/engineering"]);
    // Sam (in sales) reads it all.
    assert!(matches!(
        doc_state(&s, &sales, "sd").await.unwrap(),
        DocState::Ready
    ));
    assert!(doc_info(&s, &sales, "sd").await.unwrap().is_some());
    assert_eq!(
        read_passages(&s, &sales, "sd", Selector::Page(1), 10)
            .await
            .unwrap()
            .len(),
        1
    );
    assert_eq!(document_outline(&s, &sales, "sd").await.unwrap().len(), 1);
    assert!(
        document_picture(&s, &sales, "sd", None)
            .await
            .unwrap()
            .is_some()
    );
    // An engineer (not in sales) and Bob (denied by name) read nothing of it.
    for reader in [&engineer, &bob] {
        assert!(matches!(
            doc_state(&s, reader, "sd").await.unwrap(),
            DocState::Unknown
        ));
        assert!(doc_info(&s, reader, "sd").await.unwrap().is_none());
        for sel in [Selector::Page(1), Selector::Ords { from: 0, to: 9 }] {
            assert!(
                read_passages(&s, reader, "sd", sel, 10)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
        assert!(
            passages_on_page(&s, reader, "sd", 1, 10)
                .await
                .unwrap()
                .is_empty()
        );
        assert!(document_outline(&s, reader, "sd").await.unwrap().is_empty());
        for page in [None, Some(1)] {
            assert!(
                document_picture(&s, reader, "sd", page)
                    .await
                    .unwrap()
                    .is_none()
            );
        }
    }
    // A document row the reader can't read is Unknown even when a passage of it is
    // readable (rows out of step mid-publish never open the document).
    let mut stray = text("x0", "xd", &[], 0, "stray", unit(2));
    stray.access = vec!["company".into()];
    s.insert_many(db::PASSAGE, &[stray]).await.unwrap();
    s.put(
        db::DOCUMENT,
        "xd",
        &doc_row("xd", Some("alice"), &["person/alice"], &[]),
    )
    .await
    .unwrap();
    assert!(matches!(
        doc_state(&s, &engineer, "xd").await.unwrap(),
        DocState::Unknown
    ));
    // A document stored without an audience is its owner's alone.
    s.put(db::DOCUMENT, "od", &doc_row("od", Some("sam"), &[], &[]))
        .await
        .unwrap();
    assert!(doc_info(&s, &sales, "od").await.unwrap().is_some());
    assert!(doc_info(&s, &engineer, "od").await.unwrap().is_none());
}

/// The cache key holds the reader (paths and denials): a packed result never crosses
/// readers, and the same reader finds it again.
#[test]
fn the_cache_key_crosses_no_reader() {
    let req = |reader: Reader| Request {
        plan: QueryPlan::single("entropy"),
        scope: Scope::Library,
        reader,
        modalities: Vec::new(),
        k: 8,
        budget: Budget::default(),
        window_tokens: 100_000,
        reserve_output: 1_000,
        trace_id: "t".into(),
        caller: None,
        purpose: RetrievalPurpose::Context,
        provider: None,
    };
    let key = |r: Reader| cache_key(&req(r), "p", Some("e".into()), "g");
    let alice = person_reader("alice", &["company"]);
    assert_eq!(key(alice.clone()), key(alice.clone()));
    // The same arrays in another order, with repeats, are the same reader.
    assert_eq!(
        key(alice.clone()),
        key(Reader::sorted(
            vec!["person/alice".into(), "company".into(), "company".into()],
            vec!["person/alice".into(), "company".into()],
        ))
    );
    assert_ne!(key(alice.clone()), key(person_reader("bob", &["company"])));
    // Same paths, other denials (alice's agent: its deny holds `agent/<id>`).
    let agent = Reader::sorted(
        vec!["company".into(), "person/alice".into()],
        vec!["agent/a1".into(), "company".into(), "person/alice".into()],
    );
    assert_ne!(key(alice.clone()), key(agent));
    // The rest of the key still counts.
    assert_ne!(
        cache_key(&req(alice.clone()), "p", Some("e".into()), "g"),
        cache_key(&req(alice), "p", Some("e".into()), "g2")
    );
}

/// The rewriter's names come from a statement with the reader's two conjuncts: denials
/// count, and a document stored without an audience is its owner's.
#[tokio::test]
async fn the_rewriters_names_respect_denials_and_owners() {
    let s = Store::memory().await.unwrap();
    for d in [
        doc_row("d1", None, &["company"], &[]),
        doc_row("d2", None, &["company"], &["person/bob"]),
        doc_row("d3", Some("bob"), &[], &[]),
        doc_row("d4", Some("alice"), &[], &[]),
    ] {
        s.put(db::DOCUMENT, &d.id.clone(), &d).await.unwrap();
    }
    let library = Resolved {
        library: true,
        ..Default::default()
    };
    let bob = person_reader("bob", &["company"]);
    assert_eq!(scope_names(&s, &library, &bob).await, ["d1.md", "d3.md"]);
    let alice = person_reader("alice", &["company"]);
    assert_eq!(
        scope_names(&s, &library, &alice).await,
        ["d1.md", "d2.md", "d4.md"]
    );
}
