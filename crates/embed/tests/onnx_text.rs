//! The ONNX text embedder through the public API. Without a model only the load errors
//! and selection are checked; with `MARGIN_REAL_MODEL_TESTS=1` and a downloaded ONNX
//! model (`MARGIN_EMBED_ONNX_DIR`, default `~/Library/Caches/margin-embed-test/onnx-q8`,
//! or `$XDG_CACHE_HOME`/`~/.cache` on Linux; the format comes from
//! `MARGIN_EMBED_BACKEND`, default `onnx-q8`) the real model is loaded on the CPU and
//! checked. Download one with
//! `MARGIN_EMBED_BACKEND=onnx-q8 MARGIN_EMBED_DOWNLOAD_TO=<dir> cargo test -p margin-embed
//! --features onnx real_download_when_asked`.
#![cfg(any(target_os = "linux", feature = "onnx"))]

use std::path::PathBuf;
use std::sync::OnceLock;

use margin_embed::{
    Backend, DIM, EmbedError, Embedder, ModelFiles, OnnxDtype, OnnxEmbedder, OnnxOptions, Purpose,
    cosine, load_embedder,
};

#[test]
fn loading_without_a_download_says_so() {
    let dir = std::env::temp_dir().join(format!("margin-onnx-none-{}", std::process::id()));
    let files = ModelFiles::with_backend(&dir, Backend::Onnx(OnnxDtype::Q8));
    assert!(matches!(
        OnnxEmbedder::load(&files, &OnnxOptions::default()),
        Err(EmbedError::NotDownloaded)
    ));
    assert!(matches!(
        load_embedder(&files),
        Err(EmbedError::NotDownloaded)
    ));
    assert!(OnnxOptions::default().threads() >= 1);
    let opts = OnnxOptions {
        threads: 3,
        batch_tokens: 100_000,
    };
    assert_eq!((opts.threads(), opts.batch_tokens()), (3, 8192));
}

fn real_model_tests() -> bool {
    let on = std::env::var("MARGIN_REAL_MODEL_TESTS").is_ok_and(|v| v == "1");
    if !on {
        eprintln!("skipping: real-model tests need MARGIN_REAL_MODEL_TESTS=1");
    }
    on
}

fn files() -> Option<ModelFiles> {
    if !real_model_tests() {
        return None;
    }
    let backend = std::env::var("MARGIN_EMBED_BACKEND")
        .ok()
        .map_or(Backend::Onnx(OnnxDtype::Q8), |b| b.parse().unwrap());
    let dir = match std::env::var_os("MARGIN_EMBED_ONNX_DIR") {
        Some(dir) => PathBuf::from(dir),
        None => {
            let home = PathBuf::from(std::env::var_os("HOME")?);
            let cache = if cfg!(target_os = "macos") {
                home.join("Library/Caches")
            } else {
                std::env::var_os("XDG_CACHE_HOME").map_or(home.join(".cache"), PathBuf::from)
            };
            cache.join("margin-embed-test").join(backend.name())
        }
    };
    let files = ModelFiles::with_backend(dir, backend);
    if files.is_complete() {
        Some(files)
    } else {
        eprintln!(
            "skipping: no downloaded {backend} model in {} (set MARGIN_EMBED_ONNX_DIR)",
            files.dir.display()
        );
        None
    }
}

fn model() -> Option<&'static OnnxEmbedder> {
    static MODEL: OnceLock<Option<OnnxEmbedder>> = OnceLock::new();
    MODEL
        .get_or_init(|| Some(OnnxEmbedder::load(&files()?, &OnnxOptions::default()).unwrap()))
        .as_ref()
}

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|t| t.to_string()).collect()
}

#[test]
fn real_model_vectors_are_unit_length_and_rank_the_relevant_passage_first() {
    let Some(model) = model() else { return };
    let docs = strings(&[
        "The Fourier series writes a periodic function as a sum of sines and cosines.",
        "Photosynthesis turns light, water and carbon dioxide into sugar and oxygen.",
        "TCP congestion control slows the sender down when packets are lost.",
    ]);
    let d = model.embed(&docs, Purpose::DOCUMENT).unwrap();
    let q = model
        .embed(
            &strings(&[
                "How do plants make food from sunlight?",
                "decomposing a periodic signal into sinusoids",
            ]),
            Purpose::Query,
        )
        .unwrap();
    for v in d.iter().chain(&q) {
        assert_eq!(v.len(), DIM);
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((norm - 1.0).abs() < 1e-3, "norm {norm}");
    }
    assert!(cosine(&q[0], &d[1]) > cosine(&q[0], &d[0]));
    assert!(cosine(&q[0], &d[1]) > cosine(&q[0], &d[2]));
    assert!(cosine(&q[1], &d[0]) > cosine(&q[1], &d[1]));
    assert!(cosine(&q[1], &d[0]) > cosine(&q[1], &d[2]));
    assert_eq!(model.version(), files().unwrap().backend.embedder_version());
}

#[test]
fn real_model_batches_match_one_by_one_and_pipelined() {
    let Some(model) = model() else { return };
    let texts = strings(&[
        "short",
        &"A heat engine takes heat from a hot reservoir and rejects the rest. ".repeat(20),
        "",
        "Ünïcödé ﬁ ligature and ① circled 東京",
        &"word ".repeat(3000),
    ]);
    let together = model.embed(&texts, Purpose::DOCUMENT).unwrap();
    for (i, t) in texts.iter().enumerate() {
        let alone = model
            .embed(std::slice::from_ref(t), Purpose::DOCUMENT)
            .unwrap();
        let c = cosine(&alone[0], &together[i]);
        assert!(
            c > 0.9995,
            "input {i}: padding changed the vector (cos {c})"
        );
    }
    let inputs: Vec<(String, Purpose)> = texts
        .iter()
        .map(|t| (t.clone(), Purpose::DOCUMENT))
        .collect();
    let mut piped = Vec::new();
    model
        .embed_pipelined(&inputs, 2, &mut |_, v| {
            piped.extend(v);
            Ok(())
        })
        .unwrap();
    for (a, b) in piped.iter().zip(&together) {
        assert!(cosine(a, b) > 0.9995);
    }
    assert!(model.count_tokens("Hello, world.") > 0);
}

/// Prints the three graphs' inputs and outputs (what the media area writes into
/// `onnx/mod.rs` for the encoders) and checks the text graph's.
#[test]
fn real_encoder_graphs_are_described() {
    let Some(files) = files() else { return };
    for graph in ["model", "vision_encoder", "audio_encoder"] {
        let path = files.graph_path(graph).unwrap();
        let (inputs, outputs) = margin_embed::onnx::describe_graph(&path).unwrap();
        eprintln!("{graph}:");
        for p in &inputs {
            eprintln!("  input  {} {} {:?}", p.name, p.dtype, p.shape);
        }
        for p in &outputs {
            eprintln!("  output {} {} {:?}", p.name, p.dtype, p.shape);
        }
        if graph == "model" {
            let names: Vec<&str> = inputs.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"input_ids") && names.contains(&"attention_mask"));
            assert!(outputs.iter().any(|p| p.name == "sentence_embedding"));
        }
    }
}
