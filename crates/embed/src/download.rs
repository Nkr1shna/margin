//! Fetching the text model's files from Hugging Face. The file list comes from the
//! repo's tree API, so every file inside `EmbeddingGemma2Text.mlpackage` is fetched
//! whatever the package holds; the audio and vision encoders are skipped.
//!
//! Each file streams into `{path}.part` and is renamed when its size checks out. The
//! list of files (with sizes) is written to `files.json` last, so a folder without it
//! is an unfinished download.

use std::path::{Path, PathBuf};

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;

use crate::{EmbedError, REPO};

/// The Core ML package the text functions live in.
pub(crate) const TEXT_PACKAGE: &str = "EmbeddingGemma2Text.mlpackage";
/// Where `CoreMlEmbedder::load` keeps the compiled package.
pub(crate) const COMPILED: &str = "EmbeddingGemma2Text.mlmodelc";
/// Top-level files the text path reads besides the package.
const TOP_FILES: &[&str] = &["config.json", "tokenizer.json", "embeddings.bf16"];
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

/// The files the text model needs, from `{hf_base}/api/models/{REPO}/tree/main`.
pub(crate) async fn remote_files(
    client: &reqwest::Client,
    hf_base: &str,
) -> Result<Vec<RemoteFile>, EmbedError> {
    let url = format!(
        "{}/api/models/{REPO}/tree/main?recursive=true",
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
    let package = format!("{TEXT_PACKAGE}/");
    let files: Vec<RemoteFile> = tree
        .into_iter()
        .filter(|e| e.kind == "file")
        .filter(|e| TOP_FILES.contains(&e.path.as_str()) || e.path.starts_with(&package))
        .map(|e| RemoteFile {
            path: e.path,
            size: e.size,
        })
        .collect();
    for needed in TOP_FILES {
        if !files.iter().any(|f| f.path == *needed) {
            return Err(EmbedError::Download(format!("{REPO} has no {needed}")));
        }
    }
    if !files.iter().any(|f| f.path.starts_with(&package)) {
        return Err(EmbedError::Download(format!(
            "{REPO} has no {TEXT_PACKAGE}"
        )));
    }
    Ok(files)
}

pub(crate) async fn download(
    dir: &Path,
    client: &reqwest::Client,
    hf_base: &str,
    progress: &(dyn Fn(u64, u64) + Send + Sync),
) -> Result<(), EmbedError> {
    let files = remote_files(client, hf_base).await?;
    let total: u64 = files.iter().map(|f| f.size).sum();
    tokio::fs::create_dir_all(dir).await?;
    // Not complete until the new list is written.
    remove_if_exists(&dir.join(MANIFEST)).await?;

    let mut received = 0u64;
    let mut fetched_any = false;
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
        let url = format!("{}/{REPO}/resolve/main/{}", base(hf_base), file.path);
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
        fetched_any = true;
    }

    // New package files need a new compile.
    if fetched_any {
        remove_if_exists(&dir.join(COMPILED)).await?;
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
pub(crate) fn is_complete(dir: &Path) -> bool {
    let Ok(bytes) = std::fs::read(dir.join(MANIFEST)) else {
        return false;
    };
    let Ok(files) = serde_json::from_slice::<Vec<RemoteFile>>(&bytes) else {
        return false;
    };
    let listed = |name: &str| files.iter().any(|f| f.path == name);
    TOP_FILES.iter().all(|f| listed(f))
        && files
            .iter()
            .any(|f| f.path.starts_with(&format!("{TEXT_PACKAGE}/")))
        && files.iter().all(|f| {
            file_path(dir, &f.path)
                .ok()
                .and_then(|p| std::fs::metadata(p).ok())
                .is_some_and(|m| m.is_file() && m.len() == f.size)
        })
}

/// Bytes of everything under `dir` (files and the compiled model).
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

    use crate::ModelFiles;

    use super::*;

    /// A fake Hugging Face repo: the tree listing plus small files, including the audio
    /// package a text download must skip.
    struct Repo {
        files: Vec<(String, Vec<u8>)>,
        /// Paths the server answers with 404.
        missing: Vec<String>,
        /// How many file requests it served.
        fetches: AtomicUsize,
    }

    fn fake_files() -> Vec<(String, Vec<u8>)> {
        [
            ("config.json", b"{\"hidden_size\": 512}".to_vec()),
            ("tokenizer.json", vec![b't'; 3000]),
            ("embeddings.bf16", vec![7u8; 100_000]),
            ("README.md", b"# card".to_vec()),
            (
                "EmbeddingGemma2Text.mlpackage/Manifest.json",
                b"{}".to_vec(),
            ),
            (
                "EmbeddingGemma2Text.mlpackage/Data/com.apple.CoreML/model.mlmodel",
                vec![1u8; 5000],
            ),
            (
                "EmbeddingGemma2Text.mlpackage/Data/com.apple.CoreML/weights/weight.bin",
                vec![2u8; 70_000],
            ),
            (
                "EmbeddingGemma2Audio.mlpackage/Manifest.json",
                b"{}".to_vec(),
            ),
            ("position_embeddings.f16", vec![3u8; 900]),
        ]
        .into_iter()
        .map(|(p, b)| (p.to_string(), b))
        .collect()
    }

    async fn serve(repo: Arc<Repo>) -> String {
        async fn tree(State(repo): State<Arc<Repo>>) -> axum::Json<serde_json::Value> {
            let mut entries = vec![
                serde_json::json!({"type": "directory", "path": "EmbeddingGemma2Text.mlpackage", "size": 0}),
            ];
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
        let app = Router::new()
            .route(&format!("/api/models/{REPO}/tree/main"), get(tree))
            .route(&format!("/{REPO}/resolve/main/{{*path}}"), get(file))
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

    fn text_bytes() -> u64 {
        fake_files()
            .iter()
            .filter(|(p, _)| TOP_FILES.contains(&p.as_str()) || p.starts_with(TEXT_PACKAGE))
            .map(|(_, b)| b.len() as u64)
            .sum()
    }

    #[tokio::test]
    async fn downloads_only_the_text_model_with_progress() {
        let repo = Arc::new(Repo {
            files: fake_files(),
            missing: vec![],
            fetches: 0.into(),
        });
        let base = serve(repo.clone()).await;
        let client = reqwest::Client::new();
        let files = ModelFiles::new(temp_dir("download"));

        assert_eq!(
            ModelFiles::remote_size(&client, &base).await.unwrap(),
            text_bytes()
        );
        assert!(!files.is_complete());

        let seen = Mutex::new(Vec::new());
        files
            .download(&client, &base, &|received, total| {
                seen.lock().unwrap().push((received, total))
            })
            .await
            .unwrap();
        let seen = seen.into_inner().unwrap();
        assert_eq!(seen.first(), Some(&(0, text_bytes())));
        assert_eq!(seen.last(), Some(&(text_bytes(), text_bytes())));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));

        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), 6);
        for (path, bytes) in fake_files() {
            let on_disk = std::fs::read(files.dir.join(&path)).ok();
            if TOP_FILES.contains(&path.as_str()) || path.starts_with(TEXT_PACKAGE) {
                assert_eq!(on_disk.as_ref(), Some(&bytes), "{path}");
            } else {
                assert_eq!(on_disk, None, "{path} should be skipped");
            }
        }
        assert!(files.size_on_disk() >= text_bytes());

        // A missing file makes it incomplete; downloading again fetches only that file.
        std::fs::remove_file(files.dir.join("embeddings.bf16")).unwrap();
        assert!(!files.is_complete());
        files.download(&client, &base, &|_, _| {}).await.unwrap();
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), 7);

        files.remove().unwrap();
        assert!(!files.dir.exists());
        assert!(!files.is_complete());
        assert_eq!(files.size_on_disk(), 0);
        files.remove().unwrap();
    }

    #[tokio::test]
    async fn a_failed_download_is_never_complete() {
        let weights = "EmbeddingGemma2Text.mlpackage/Data/com.apple.CoreML/weights/weight.bin";
        let repo = Arc::new(Repo {
            files: fake_files(),
            missing: vec![weights.to_string()],
            fetches: 0.into(),
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
    async fn a_repo_without_the_text_model_is_an_error() {
        let files: Vec<_> = fake_files()
            .into_iter()
            .filter(|(p, _)| p != "tokenizer.json")
            .collect();
        let repo = Arc::new(Repo {
            files,
            missing: vec![],
            fetches: 0.into(),
        });
        let base = serve(repo).await;
        let err = ModelFiles::remote_size(&reqwest::Client::new(), &base)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("tokenizer.json"), "{err}");
    }

    /// Downloads the real model from Hugging Face when `MARGIN_EMBED_DOWNLOAD_TO` names a
    /// folder (e.g. `~/Library/Caches/margin-embed-test/model` for the Core ML tests).
    #[tokio::test]
    async fn real_download_when_asked() {
        let Some(dir) = std::env::var_os("MARGIN_EMBED_DOWNLOAD_TO") else {
            eprintln!("skipping: MARGIN_EMBED_DOWNLOAD_TO is not set");
            return;
        };
        let client = reqwest::Client::new();
        let files = ModelFiles::new(PathBuf::from(dir));
        let total = ModelFiles::remote_size(&client, crate::HF_BASE)
            .await
            .unwrap();
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

    #[test]
    fn file_paths_stay_inside_the_folder() {
        let dir = Path::new("/models");
        assert!(file_path(dir, "a/b.bin").is_ok());
        assert!(file_path(dir, "../evil").is_err());
        assert!(file_path(dir, "/etc/passwd").is_err());
    }
}
