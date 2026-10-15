//! The ONNX cross-encoder through the public API (R2, `docs/r2-brief.md` D5). Without a
//! model only the load errors are checked. With `MARGIN_REAL_MODEL=1` (or
//! `MARGIN_REAL_MODEL_TESTS=1`) the real model (`MARGIN_RERANKER_MODEL`, default
//! `ms-marco-minilm-l6-v2`) is downloaded from Hugging Face at its pinned revision into
//! `MARGIN_RERANKER_DIR` (default `~/Library/Caches/margin-embed-test/rerankers` on
//! macOS, `$XDG_CACHE_HOME` or `~/.cache` elsewhere) unless it is there already, loaded
//! on the CPU, checked to rank the relevant passage first, and measured: the resident
//! memory it adds (budget 400 MB, the 4–8 GB guests of `docs/plan-linux-service.md` §3)
//! and the milliseconds per pair, printed into the test log. Run with
//! `MARGIN_REAL_MODEL=1 cargo test -p margin-embed --features onnx --test onnx_rerank --
//! --nocapture` (no `--features` on Linux).
#![cfg(any(target_os = "linux", feature = "onnx"))]

use std::path::PathBuf;
use std::time::Instant;

use margin_embed::onnx::OnnxReranker;
use margin_embed::{EmbedError, OnnxOptions, Reranker, RerankerFiles, RerankerModel};

/// Most resident memory the loaded reranker may add.
const RSS_BUDGET_MB: f64 = 400.0;

#[test]
fn loading_without_a_download_says_so() {
    let dir = std::env::temp_dir().join(format!("margin-rerank-none-{}", std::process::id()));
    for model in RerankerModel::ALL {
        let files = RerankerFiles::new(&dir, model);
        assert!(!files.is_complete());
        assert!(matches!(
            OnnxReranker::load(&files, &OnnxOptions::default()),
            Err(EmbedError::NotDownloaded)
        ));
        assert_eq!(
            files.graph_path(),
            files.dir.join("onnx/model_quantized.onnx")
        );
        assert_eq!(model.revision().len(), 40);
    }
}

fn real_model_tests() -> bool {
    let on = ["MARGIN_REAL_MODEL", "MARGIN_REAL_MODEL_TESTS"]
        .iter()
        .any(|k| std::env::var(k).is_ok_and(|v| v == "1"));
    if !on {
        eprintln!("skipping: real-model tests need MARGIN_REAL_MODEL=1");
    }
    on
}

fn cache_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("MARGIN_RERANKER_DIR") {
        return PathBuf::from(dir);
    }
    let home = PathBuf::from(std::env::var_os("HOME").expect("HOME"));
    let cache = if cfg!(target_os = "macos") {
        home.join("Library/Caches")
    } else {
        std::env::var_os("XDG_CACHE_HOME").map_or(home.join(".cache"), PathBuf::from)
    };
    cache.join("margin-embed-test").join("rerankers")
}

/// The real model's files, downloaded first if they aren't there.
fn files() -> Option<RerankerFiles> {
    if !real_model_tests() {
        return None;
    }
    let model = std::env::var("MARGIN_RERANKER_MODEL").map_or(RerankerModel::DEFAULT, |n| {
        RerankerModel::from_name(&n).unwrap_or_else(|| panic!("no reranker model {n}"))
    });
    let files = RerankerFiles::new(&cache_dir(), model);
    if !files.is_complete() {
        eprintln!(
            "downloading {} at {} into {}",
            model.repo(),
            model.revision(),
            files.dir.display()
        );
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime
            .block_on(files.download(&reqwest::Client::new(), margin_embed::HF_BASE, &|_, _| {}))
            .unwrap();
    }
    assert!(files.is_complete());
    Some(files)
}

/// This process's resident memory, in MB.
fn rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        let status = std::fs::read_to_string("/proc/self/status").unwrap();
        let kb: f64 = status
            .lines()
            .find_map(|l| l.strip_prefix("VmRSS:"))
            .and_then(|v| v.split_whitespace().next())
            .and_then(|v| v.parse().ok())
            .unwrap();
        kb / 1024.0
    }
    #[cfg(not(target_os = "linux"))]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p", &std::process::id().to_string()])
            .output()
            .unwrap();
        let kb: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().unwrap();
        kb / 1024.0
    }
}

fn strings(texts: &[&str]) -> Vec<String> {
    texts.iter().map(|t| t.to_string()).collect()
}

#[test]
fn real_model_ranks_the_relevant_passage_first_within_the_memory_budget() {
    let Some(files) = files() else { return };
    // `MARGIN_RERANKER_THREADS` measures another intra-op thread count.
    let opts = OnnxOptions {
        threads: std::env::var("MARGIN_RERANKER_THREADS")
            .ok()
            .and_then(|t| t.parse().ok())
            .unwrap_or(0),
        ..OnnxOptions::default()
    };
    let before = rss_mb();
    let reranker = OnnxReranker::load(&files, &opts).unwrap();
    assert_eq!(reranker.name(), files.model.version());
    assert!(!reranker.is_loaded(), "the graph loads on first use");

    let passages = strings(&[
        "TCP congestion control slows the sender down when packets are lost.",
        "The Gibbs phenomenon is the overshoot of a Fourier series near a jump \
         discontinuity; it does not vanish as more terms are added.",
        "Photosynthesis turns light, water and carbon dioxide into sugar and oxygen.",
        "A Fourier series writes a periodic function as a sum of sines and cosines.",
    ]);
    let scores = reranker
        .score(
            "why does a fourier series overshoot near a jump?",
            &passages,
        )
        .unwrap();
    eprintln!("scores: {scores:?}");
    assert_eq!(scores.len(), passages.len());
    let best = (0..scores.len())
        .max_by(|a, b| scores[*a].total_cmp(&scores[*b]))
        .unwrap();
    assert_eq!(best, 1, "{scores:?}");
    assert!(scores[1] > scores[0] && scores[1] > scores[2]);
    assert!(scores[3] > scores[0], "{scores:?}");
    assert!(reranker.is_loaded());

    // 24 candidates of a few hundred tokens each, as the stage hands it.
    let body = "A heat engine takes heat from a hot reservoir, turns part of it into \
                work and rejects the rest to a cold one; Carnot's bound limits the \
                share it can turn into work. ";
    let candidates: Vec<String> = (0..24)
        .map(|i| format!("Passage {i}. {}", body.repeat(1 + i % 6)))
        .collect();
    let query = "what limits the efficiency of a heat engine";
    reranker.score(query, &candidates).unwrap();
    let start = Instant::now();
    let runs = 3;
    for _ in 0..runs {
        let s = reranker.score(query, &candidates).unwrap();
        assert_eq!(s.len(), 24);
        assert!(s.iter().all(|x| x.is_finite()));
    }
    let ms_per_pair = start.elapsed().as_secs_f64() * 1000.0 / (runs * 24) as f64;
    let added = rss_mb() - before;
    eprintln!(
        "reranker {}: {ms_per_pair:.1} ms per pair ({} intra-op threads), {added:.0} MB resident added (budget {RSS_BUDGET_MB} MB)",
        reranker.name(),
        opts.threads()
    );
    assert!(
        added <= RSS_BUDGET_MB,
        "the reranker added {added:.0} MB, over the {RSS_BUDGET_MB} MB budget"
    );

    // Batched or one at a time, a pair scores nearly the same: the int8 export
    // quantizes activations dynamically, over the whole batch, so a batch's other rows
    // nudge a score (~0.1 logit measured, on a scale where relevant and unrelated
    // passages sit ~15 apart). The same candidates always batch the same way, so a
    // ranking is reproducible.
    let alone = reranker
        .score(query, std::slice::from_ref(&candidates[5]))
        .unwrap();
    let together = reranker.score(query, &candidates).unwrap();
    eprintln!(
        "alone {} vs batched {} (diff {:.3})",
        alone[0],
        together[5],
        (alone[0] - together[5]).abs()
    );
    assert_eq!(together, reranker.score(query, &candidates).unwrap());
    assert!(
        (alone[0] - together[5]).abs() < 0.3,
        "padding changed the score: {} vs {}",
        alone[0],
        together[5]
    );

    // Unloading frees the graph; the next call loads it again.
    assert!(reranker.unload());
    assert!(!reranker.is_loaded());
    assert_eq!(reranker.score(query, &candidates[..2]).unwrap().len(), 2);
}
