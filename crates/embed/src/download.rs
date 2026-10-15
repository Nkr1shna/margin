//! Fetching the model's files from Hugging Face in one download with one progress
//! total: from `ONNX_REPO`, the tokenizer, `config.json`, the processor configs, and the
//! three graphs (`onnx/model`, `onnx/vision_encoder`, `onnx/audio_encoder`) in the
//! backend's format (`OnnxDtype`, its `Manifest` below), each with its external data
//! file(s).
//!
//! The file list comes from the repo's tree API; anything else in the repo (README, other
//! formats, …) is skipped. Each file streams into `{path}.part` and is renamed when its
//! size checks out. The list of files (with sizes) is written to `files.json` last, so a
//! folder without it is an unfinished download.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{Backend, EmbedError, OnnxDtype, RerankerModel};

/// The ONNX export's graphs: the text model (which also takes the encoders' feature
/// rows), the image encoder and the audio encoder.
pub(crate) const ONNX_TEXT_GRAPH: &str = "model";
pub(crate) const ONNX_VISION_GRAPH: &str = "vision_encoder";
pub(crate) const ONNX_AUDIO_GRAPH: &str = "audio_encoder";
pub(crate) const ONNX_GRAPHS: &[&str] = &[ONNX_TEXT_GRAPH, ONNX_VISION_GRAPH, ONNX_AUDIO_GRAPH];
/// Top-level files the ONNX backend can't do without.
const ONNX_TOP_FILES: &[&str] = &["config.json", "tokenizer.json"];
/// Top-level files fetched when the repo has them (the image and audio preprocessing
/// settings the media path reads).
const ONNX_OPTIONAL_FILES: &[&str] = &["processor_config.json", "preprocessor_config.json"];

/// A graph's file in the repo (and in the model folder): `onnx/{graph}{suffix}.onnx`.
pub(crate) fn onnx_graph(graph: &str, dtype: OnnxDtype) -> String {
    format!("onnx/{graph}{}.onnx", dtype.file_suffix())
}

/// Whether `path` is `graph`'s file in `dtype` or one of its external data files
/// (`….onnx_data`, `….onnx_data_1`, …).
fn is_graph_file(path: &str, graph: &str, dtype: OnnxDtype) -> bool {
    let file = onnx_graph(graph, dtype);
    path == file
        || path
            .strip_prefix(&file)
            .is_some_and(|rest| rest == "_data" || rest.starts_with("_data_"))
}

/// Which repo files a download fetches and which it can't do without: the embedding
/// model of a backend, or a reranker model (R2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Manifest {
    Model(Backend),
    Reranker(RerankerModel),
}

impl Manifest {
    fn repo(self) -> &'static str {
        match self {
            Manifest::Model(backend) => backend.repo(),
            Manifest::Reranker(model) => model.repo(),
        }
    }

    /// The repo revision files are listed and fetched at: the embedding model follows
    /// `main`; a reranker is pinned (`RerankerModel::revision`).
    fn revision(self) -> &'static str {
        match self {
            Manifest::Model(_) => "main",
            Manifest::Reranker(model) => model.revision(),
        }
    }

    /// Whether a download fetches `path`.
    fn wanted(self, path: &str) -> bool {
        match self {
            Manifest::Model(Backend::Onnx(dtype)) => {
                ONNX_TOP_FILES.contains(&path)
                    || ONNX_OPTIONAL_FILES.contains(&path)
                    || ONNX_GRAPHS.iter().any(|g| is_graph_file(path, g, dtype))
            }
            Manifest::Reranker(model) => model.files().contains(&path),
        }
    }

    /// The first required file `files` lacks.
    fn missing(self, files: &[RemoteFile]) -> Option<String> {
        let listed = |name: &str| files.iter().any(|f| f.path == name);
        match self {
            Manifest::Model(Backend::Onnx(dtype)) => ONNX_TOP_FILES
                .iter()
                .map(|f| f.to_string())
                .chain(ONNX_GRAPHS.iter().map(|g| onnx_graph(g, dtype)))
                .find(|f| !listed(f)),
            Manifest::Reranker(model) => model
                .files()
                .iter()
                .find(|f| !listed(f))
                .map(|f| f.to_string()),
        }
    }
}

/// Written after every file is in place.
const MANIFEST: &str = "files.json";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(crate) struct RemoteFile {
    pub path: String,
    pub size: u64,
}

#[derive(Deserialize)]
struct TreeEntry {
    #[serde(rename = "type")]
    kind: String,
    path: String,
    #[serde(default)]
    size: u64,
}

/// The files the model needs, from `{hf_base}/api/models/{repo}/tree/{revision}`.
pub(crate) async fn remote_files(
    client: &reqwest::Client,
    hf_base: &str,
    manifest: Manifest,
) -> Result<Vec<RemoteFile>, EmbedError> {
    let repo = manifest.repo();
    let revision = manifest.revision();
    let url = format!(
        "{}/api/models/{repo}/tree/{revision}?recursive=true",
        base(hf_base)
    );
    let response = client
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| EmbedError::Download(e.to_string()))?;
    let tree: Vec<TreeEntry> = response
        .json()
        .await
        .map_err(|e| EmbedError::Download(format!("unexpected file list: {e}")))?;
    let files: Vec<RemoteFile> = tree
        .into_iter()
        .filter(|e| e.kind == "file")
        .filter(|e| manifest.wanted(&e.path))
        .map(|e| RemoteFile {
            path: e.path,
            size: e.size,
        })
        .collect();
    if let Some(needed) = manifest.missing(&files) {
        return Err(EmbedError::Download(format!("{repo} has no {needed}")));
    }
    Ok(files)
}

pub(crate) async fn download(
    dir: &Path,
    manifest: Manifest,
    client: &reqwest::Client,
    hf_base: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), EmbedError> {
    let repo = manifest.repo();
    let files = remote_files(client, hf_base, manifest).await?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    tokio::fs::create_dir_all(dir).await?;
    // Not complete until the new list is written.
    remove_if_exists(&dir.join(MANIFEST)).await?;

    let mut received = 0u64;
    progress(received, total);
    for file in &files {
        let target = file_path(dir, &file.path)?;
        // Kept from an earlier, interrupted download.
        if tokio::fs::metadata(&target)
            .await
            .is_ok_and(|m| m.is_file() && m.len() == file.size)
        {
            received += file.size;
            progress(received, total);
            continue;
        }
        if let Some(parent) = target.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let url = format!(
            "{}/{repo}/resolve/{}/{}",
            base(hf_base),
            manifest.revision(),
            file.path
        );
        let partial = part_path(&target);
        let start = received;
        let result = fetch(client, &url, &partial, |n| {
            received += n;
            progress(received, total);
        })
        .await;
        let written = match result {
            Ok(written) => written,
            Err(e) => {
                let _ = tokio::fs::remove_file(&partial).await;
                return Err(e);
            }
        };
        if written != file.size {
            let _ = tokio::fs::remove_file(&partial).await;
            return Err(EmbedError::Download(format!(
                "{} is {written} bytes, expected {}",
                file.path, file.size
            )));
        }
        received = start + written;
        tokio::fs::rename(&partial, &target).await?;
    }

    let manifest = dir.join(MANIFEST);
    let partial = part_path(&manifest);
    let json =
        serde_json::to_vec_pretty(&files).map_err(|e| EmbedError::Download(e.to_string()))?;
    tokio::fs::write(&partial, json).await?;
    tokio::fs::rename(&partial, &manifest).await?;
    Ok(())
}

/// Streams `url` into `to`, calling `on_bytes` with each piece's length. Returns the
/// bytes written.
async fn fetch(
    client: &reqwest::Client,
    url: &str,
    to: &Path,
    mut on_bytes: impl FnMut(u64),
) -> Result<u64, EmbedError> {
    let response = client
        .get(url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| EmbedError::Download(e.to_string()))?;
    let mut out = tokio::fs::File::create(to).await?;
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(piece) = stream.next().await {
        let piece = piece.map_err(|e| EmbedError::Download(e.to_string()))?;
        out.write_all(&piece).await?;
        written += piece.len() as u64;
        on_bytes(piece.len() as u64);
    }
    out.flush().await?;
    out.sync_all().await?;
    Ok(written)
}

/// Whether `files.json` exists and every file it lists is present at its size.
pub(crate) fn is_complete(dir: &Path, manifest: Manifest) -> bool {
    let Ok(bytes) = std::fs::read(dir.join(MANIFEST)) else {
        return false;
    };
    let Ok(files) = serde_json::from_slice::<Vec<RemoteFile>>(&bytes) else {
        return false;
    };
    manifest.missing(&files).is_none()
        && files.iter().all(|f| {
            file_path(dir, &f.path)
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.is_file() && m.len() == f.size)
        })
}

/// Bytes of everything under `dir`.
pub(crate) fn size_on_disk(dir: &Path) -> u64 {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .flatten()
            .map(|entry| match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&entry.path()),
                Ok(t) if t.is_file() => entry.metadata().map(|m| m.len()).unwrap_or(0),
                _ => 0,
            })
            .sum()
    }
    walk(dir)
}

pub(crate) fn remove(dir: &Path) -> Result<(), EmbedError> {
    match std::fs::remove_dir_all(dir) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
        _ => Ok(()),
    }
}

fn base(hf_base: &str) -> &str {
    hf_base.trim_end_matches('/')
}

/// `dir/path`, refusing paths that would leave `dir`.
fn file_path(dir: &Path, path: &str) -> Result<PathBuf, EmbedError> {
    let relative = Path::new(path);
    let plain = relative
        .components()
        .all(|c| matches!(c, std::path::Component::Normal(_)));
    if !plain {
        return Err(EmbedError::Download(format!("unexpected file path {path}")));
    }
    Ok(dir.join(relative))
}

fn part_path(target: &Path) -> PathBuf {
    let mut name = target.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    target.with_file_name(name)
}

impl RerankerModel {
    /// The commit of `repo()` its files are fetched at: the Xenova exports checked for
    /// R2 (2026-10-14), each with `onnx/model_quantized.onnx` (dynamic int8, 23.1 MB and
    /// 279.3 MB), `tokenizer.json` and `config.json`. Pinned so a re-export upstream
    /// can't change the scores under an unchanged `version()`.
    pub fn revision(self) -> &'static str {
        match self {
            RerankerModel::MsMarcoMiniLmL6 => "a09144355adeed5f58c8ed011d209bf8ee5a1fec",
            RerankerModel::BgeRerankerBase => "280bcc27a84e0b898c251e06fddb25171bd9b101",
        }
    }
}

impl crate::RerankerFiles {
    /// Bytes the download takes on disk (0 when there is none).
    pub fn size_on_disk(&self) -> u64 {
        size_on_disk(&self.dir)
    }
}

async fn remove_if_exists(path: &Path) -> Result<(), EmbedError> {
    let result = match tokio::fs::metadata(path).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(path).await,
        Ok(_) => tokio::fs::remove_file(path).await,
        Err(_) => return Ok(()),
    };
    result.map_err(EmbedError::from)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use axum::Router;
    use axum::extract::{Path as UrlPath, State};
    use axum::http::StatusCode;
    use axum::routing::get;

    use crate::{ModelFiles, ONNX_REPO};

    use super::*;

    /// A fake Hugging Face repo: the tree listing plus small files: every model part, and
    /// files a download must skip.
    struct Repo {
        files: Vec<(String, Vec<u8>)>,
        /// Paths the server answers with 404.
        missing: Vec<String>,
        /// How many file requests it served.
        fetches: AtomicUsize,
        /// The repo id it answers for.
        name: &'static str,
    }

    async fn serve(repo: Arc<Repo>) -> String {
        async fn tree(State(repo): State<Arc<Repo>>) -> axum::Json<serde_json::Value> {
            let mut entries =
                vec![serde_json::json!({"type": "directory", "path": "onnx", "size": 0})];
            entries.extend(repo.files.iter().map(|(path, bytes)| {
                serde_json::json!({"type": "file", "path": path, "size": bytes.len()})
            }));
            axum::Json(serde_json::Value::Array(entries))
        }
        async fn file(
            State(repo): State<Arc<Repo>>,
            UrlPath(path): UrlPath<String>,
        ) -> Result<Vec<u8>, StatusCode> {
            repo.fetches.fetch_add(1, Ordering::SeqCst);
            if repo.missing.contains(&path) {
                return Err(StatusCode::NOT_FOUND);
            }
            let found = repo.files.iter().find(|(p, _)| *p == path);
            found.map(|(_, b)| b.clone()).ok_or(StatusCode::NOT_FOUND)
        }
        // Answers only at the revision the download should ask for: a reranker's pinned
        // commit, `main` for the embedding model.
        let revision = RerankerModel::ALL
            .into_iter()
            .find(|m| m.repo() == repo.name)
            .map_or("main", |m| m.revision());
        let app = Router::new()
            .route(
                &format!("/api/models/{}/tree/{revision}", repo.name),
                get(tree),
            )
            .route(
                &format!("/{}/resolve/{revision}/{{*path}}", repo.name),
                get(file),
            )
            .with_state(repo);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}/")
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("margin-embed-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[tokio::test]
    async fn a_failed_download_is_never_complete() {
        let weights = "onnx/model_quantized.onnx_data";
        let repo = Arc::new(Repo {
            files: fake_onnx_files(),
            missing: vec![weights.to_string()],
            fetches: 0.into(),
            name: ONNX_REPO,
        });
        let base = serve(repo).await;
        let files = ModelFiles::new(temp_dir("failed"));
        let err = files
            .download(&reqwest::Client::new(), &base, &|_, _| {})
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Download(_)), "{err:?}");
        assert!(!files.is_complete());
        assert!(!files.dir.join(weights).exists());
        assert!(!part_path(&files.dir.join(weights)).exists());
        files.remove().unwrap();
    }

    #[tokio::test]
    async fn a_repo_without_the_tokenizer_is_an_error() {
        let files: Vec<_> = fake_onnx_files()
            .into_iter()
            .filter(|(p, _)| p != "tokenizer.json")
            .collect();
        let repo = Arc::new(Repo {
            files,
            missing: vec![],
            fetches: 0.into(),
            name: ONNX_REPO,
        });
        let base = serve(repo).await;
        let err = ModelFiles::new(temp_dir("no-tokenizer"))
            .remote_bytes(&reqwest::Client::new(), &base)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tokenizer.json"), "{err}");
    }

    /// Downloads the real model from Hugging Face when `MARGIN_EMBED_DOWNLOAD_TO` names a
    /// folder (e.g. `~/.cache/margin-embed-test/onnx-q8` for the ONNX tests), for the
    /// backend `MARGIN_EMBED_BACKEND` names (the default, `onnx-q8`, when unset).
    #[tokio::test]
    async fn real_download_when_asked() {
        let Some(dir) = std::env::var_os("MARGIN_EMBED_DOWNLOAD_TO") else {
            eprintln!("skipping: MARGIN_EMBED_DOWNLOAD_TO is not set");
            return;
        };
        let backend = std::env::var("MARGIN_EMBED_BACKEND")
            .ok()
            .map_or(Backend::default(), |name| name.parse().unwrap());
        let client = reqwest::Client::new();
        let files = ModelFiles::with_backend(PathBuf::from(dir), backend);
        let total = files.remote_bytes(&client, crate::HF_BASE).await.unwrap();
        eprintln!("downloading {total} bytes to {}", files.dir.display());
        let step = AtomicUsize::new(0);
        files
            .download(&client, crate::HF_BASE, &|received, total| {
                let tenth = (received * 10 / total.max(1)) as usize;
                if step.swap(tenth, Ordering::SeqCst) != tenth {
                    eprintln!("{received} / {total}");
                }
            })
            .await
            .unwrap();
        assert!(files.is_complete());
        assert!(files.size_on_disk() >= total);
    }

    /// The ONNX repo's layout: three graphs in five formats with external data, the
    /// tokenizer and configs, and files no backend fetches.
    fn fake_onnx_files() -> Vec<(String, Vec<u8>)> {
        let mut files: Vec<(String, Vec<u8>)> = [
            ("config.json", b"{\"bos_token_id\": 2}".to_vec()),
            ("tokenizer.json", vec![b't'; 2000]),
            ("tokenizer_config.json", b"{}".to_vec()),
            ("processor_config.json", b"{}".to_vec()),
            ("preprocessor_config.json", b"{}".to_vec()),
            ("README.md", b"# card".to_vec()),
            ("onnx/model.onnx.bak", vec![1u8; 10]),
        ]
        .into_iter()
        .map(|(p, b)| (p.to_string(), b))
        .collect();
        for (n, graph) in ONNX_GRAPHS.iter().enumerate() {
            for (m, suffix) in ["", "_fp16", "_q4", "_q4f16", "_quantized"]
                .iter()
                .enumerate()
            {
                let size = 100 * (n + 1) + 10 * m;
                files.push((format!("onnx/{graph}{suffix}.onnx"), vec![n as u8; size]));
                files.push((
                    format!("onnx/{graph}{suffix}.onnx_data"),
                    vec![m as u8; size * 7],
                ));
            }
        }
        // A graph whose weights are split in two.
        files.push((
            "onnx/audio_encoder_quantized.onnx_data_1".into(),
            vec![8u8; 333],
        ));
        files
    }

    #[test]
    fn the_onnx_manifest_takes_one_format_and_its_data() {
        let q8 = Manifest::Model(Backend::Onnx(OnnxDtype::Q8));
        let fp32 = Manifest::Model(Backend::Onnx(OnnxDtype::Fp32));
        assert_eq!(
            onnx_graph("model", OnnxDtype::Q8),
            "onnx/model_quantized.onnx"
        );
        assert!(q8.wanted("onnx/model_quantized.onnx"));
        assert!(q8.wanted("onnx/model_quantized.onnx_data"));
        assert!(q8.wanted("onnx/audio_encoder_quantized.onnx_data_1"));
        assert!(q8.wanted("tokenizer.json"));
        assert!(q8.wanted("processor_config.json"));
        assert!(!q8.wanted("onnx/model.onnx"));
        assert!(!q8.wanted("onnx/model_q4.onnx_data"));
        assert!(!q8.wanted("embeddings.bf16"));
        assert!(fp32.wanted("onnx/model.onnx"));
        assert!(fp32.wanted("onnx/model.onnx_data"));
        assert!(!fp32.wanted("onnx/model.onnx.bak"));
        assert!(!fp32.wanted("onnx/model_quantized.onnx"));
        assert!(!fp32.wanted("onnx/model_fp16.onnx_data"));
    }

    #[tokio::test]
    async fn downloads_the_onnx_backend_from_its_own_repo() {
        let repo = Arc::new(Repo {
            files: fake_onnx_files(),
            missing: vec![],
            fetches: 0.into(),
            name: ONNX_REPO,
        });
        let base = serve(repo.clone()).await;
        let client = reqwest::Client::new();
        let backend = Backend::Onnx(OnnxDtype::Q8);
        let files = ModelFiles::with_backend(temp_dir("onnx-download"), backend);
        let manifest = Manifest::Model(backend);
        let wanted: Vec<(String, Vec<u8>)> = fake_onnx_files()
            .into_iter()
            .filter(|(p, _)| manifest.wanted(p))
            .collect();
        // 3 graphs + 3 data files + the split + 4 top files.
        assert_eq!(wanted.len(), 11);
        let bytes: u64 = wanted.iter().map(|(_, b)| b.len() as u64).sum();
        assert_eq!(files.remote_bytes(&client, &base).await.unwrap(), bytes);
        assert!(!files.is_complete());

        let last = Mutex::new((0, 0));
        files
            .download(&client, &base, &|r, t| *last.lock().unwrap() = (r, t))
            .await
            .unwrap();
        assert_eq!(*last.lock().unwrap(), (bytes, bytes));
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), wanted.len());
        for (path, content) in fake_onnx_files() {
            let on_disk = std::fs::read(files.dir.join(&path)).ok();
            if manifest.wanted(&path) {
                assert_eq!(on_disk.as_ref(), Some(&content), "{path}");
            } else {
                assert_eq!(on_disk, None, "{path} should be skipped");
            }
        }
        assert_eq!(
            files.graph_path("model"),
            files.dir.join("onnx/model_quantized.onnx")
        );
        // The same folder doesn't count as another format's download.
        assert!(
            !ModelFiles::with_backend(&files.dir, Backend::Onnx(OnnxDtype::Fp32)).is_complete()
        );
        // A missing data file makes it incomplete again.
        std::fs::remove_file(files.dir.join("onnx/vision_encoder_quantized.onnx_data")).unwrap();
        assert!(!files.is_complete());
        files.download(&client, &base, &|_, _| {}).await.unwrap();
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), wanted.len() + 1);
        files.remove().unwrap();
        assert!(!files.is_complete());
    }

    #[tokio::test]
    async fn an_onnx_repo_without_a_graph_in_the_format_is_an_error() {
        let files: Vec<_> = fake_onnx_files()
            .into_iter()
            .filter(|(p, _)| !p.starts_with("onnx/vision_encoder_quantized"))
            .collect();
        let repo = Arc::new(Repo {
            files,
            missing: vec![],
            fetches: 0.into(),
            name: ONNX_REPO,
        });
        let base = serve(repo).await;
        let err = ModelFiles::with_backend(temp_dir("onnx-missing"), Backend::Onnx(OnnxDtype::Q8))
            .remote_bytes(&reqwest::Client::new(), &base)
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("onnx/vision_encoder_quantized.onnx"),
            "{err}"
        );
        // The fp32 graphs are all there.
        let files = ModelFiles::with_backend(temp_dir("onnx-fp32"), Backend::Onnx(OnnxDtype::Fp32));
        assert!(
            files
                .remote_bytes(&reqwest::Client::new(), &base)
                .await
                .is_ok()
        );
    }

    /// A Xenova cross-encoder export: every format, the tokenizer files and the card.
    fn fake_reranker_files() -> Vec<(String, Vec<u8>)> {
        [
            ("config.json", b"{\"pad_token_id\": 0}".to_vec()),
            ("tokenizer.json", vec![b't'; 2000]),
            ("tokenizer_config.json", b"{}".to_vec()),
            ("special_tokens_map.json", b"{}".to_vec()),
            ("vocab.txt", vec![b'v'; 300]),
            ("README.md", b"# card".to_vec()),
            ("onnx/model.onnx", vec![1u8; 9000]),
            ("onnx/model_quantized.onnx", vec![2u8; 2300]),
            ("onnx/model_int8.onnx", vec![3u8; 2300]),
            ("onnx/model_fp16.onnx", vec![4u8; 4500]),
        ]
        .into_iter()
        .map(|(p, b)| (p.to_string(), b))
        .collect()
    }

    #[test]
    fn the_reranker_manifest_takes_the_int8_graph_and_the_tokenizer() {
        for model in RerankerModel::ALL {
            let m = Manifest::Reranker(model);
            assert_eq!(m.repo(), model.repo());
            assert_eq!(m.revision().len(), 40, "pinned to a commit");
            assert!(m.wanted("onnx/model_quantized.onnx"));
            assert!(m.wanted("tokenizer.json"));
            assert!(m.wanted("config.json"));
            assert!(!m.wanted("onnx/model.onnx"));
            assert!(!m.wanted("onnx/model_int8.onnx"));
            assert!(!m.wanted("README.md"));
        }
        assert_eq!(
            Manifest::Model(Backend::Onnx(OnnxDtype::Q8)).revision(),
            "main"
        );
        assert_eq!(
            Manifest::Reranker(RerankerModel::MsMarcoMiniLmL6).missing(&[RemoteFile {
                path: "config.json".into(),
                size: 1
            }]),
            Some("tokenizer.json".into())
        );
    }

    #[tokio::test]
    async fn a_reranker_downloads_at_its_pinned_revision_and_is_removed() {
        use crate::RerankerFiles;
        let model = RerankerModel::MsMarcoMiniLmL6;
        let repo = Arc::new(Repo {
            files: fake_reranker_files(),
            missing: vec![],
            fetches: 0.into(),
            name: model.repo(),
        });
        let base = serve(repo.clone()).await;
        let client = reqwest::Client::new();
        let models = temp_dir("reranker-download");
        let files = RerankerFiles::new(&models, model);
        assert_eq!(files.dir, models.join("reranker-ms-marco-minilm-l6-v2"));
        assert_eq!((files.is_complete(), files.size_on_disk()), (false, 0));
        // config.json + tokenizer.json + the int8 graph.
        let bytes: u64 = fake_reranker_files()
            .iter()
            .filter(|(p, _)| model.files().contains(&p.as_str()))
            .map(|(_, b)| b.len() as u64)
            .sum();
        assert_eq!(bytes, 19 + 2000 + 2300);
        assert_eq!(files.remote_size(&client, &base).await.unwrap(), bytes);

        let seen = Mutex::new(Vec::new());
        files
            .download(&client, &base, &|r, t| seen.lock().unwrap().push((r, t)))
            .await
            .unwrap();
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.first(), Some(&(0, bytes)));
        assert_eq!(seen.last(), Some(&(bytes, bytes)));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), 3);
        assert_eq!(std::fs::read(files.graph_path()).unwrap(), vec![2u8; 2300]);
        assert!(files.tokenizer_path().is_file());
        assert!(!files.dir.join("onnx/model.onnx").exists());
        assert!(files.size_on_disk() >= bytes);
        // The other model's folder is untouched and incomplete.
        assert!(!RerankerFiles::new(&models, RerankerModel::BgeRerankerBase).is_complete());

        // A lost graph: the next download fetches only it.
        std::fs::remove_file(files.graph_path()).unwrap();
        assert!(!files.is_complete());
        files.download(&client, &base, &|_, _| {}).await.unwrap();
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), 4);

        files.remove().unwrap();
        assert!(!files.is_complete());
        assert!(!files.dir.exists());
        files.remove().unwrap();
    }

    #[tokio::test]
    async fn a_reranker_repo_without_the_int8_graph_or_unreachable_is_an_error() {
        use crate::RerankerFiles;
        let model = RerankerModel::BgeRerankerBase;
        let repo = Arc::new(Repo {
            files: fake_reranker_files()
                .into_iter()
                .filter(|(p, _)| p != "onnx/model_quantized.onnx")
                .collect(),
            missing: vec![],
            fetches: 0.into(),
            name: model.repo(),
        });
        let base = serve(repo).await;
        let files = RerankerFiles::new(&temp_dir("reranker-missing"), model);
        let client = reqwest::Client::new();
        let err = files.remote_size(&client, &base).await.unwrap_err();
        assert!(
            err.to_string().contains("onnx/model_quantized.onnx"),
            "{err}"
        );
        assert!(files.download(&client, &base, &|_, _| {}).await.is_err());
        assert!(!files.is_complete());

        // A file the listing promised but the server can't give leaves no partial file
        // and no finished download.
        let repo = Arc::new(Repo {
            files: fake_reranker_files(),
            missing: vec!["onnx/model_quantized.onnx".into()],
            fetches: 0.into(),
            name: model.repo(),
        });
        let base = serve(repo).await;
        let err = files
            .download(&client, &base, &|_, _| {})
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Download(_)), "{err}");
        assert!(!files.is_complete());
        assert!(!files.dir.join("onnx/model_quantized.onnx.part").exists());
        files.remove().unwrap();
    }

    #[test]
    fn file_paths_stay_inside_the_folder() {
        let dir = Path::new("/models");
        assert!(file_path(dir, "a/b.bin").is_ok());
        assert!(file_path(dir, "../evil").is_err());
        assert!(file_path(dir, "/etc/passwd").is_err());
    }
}
