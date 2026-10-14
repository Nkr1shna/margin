//! Fetching the model's files from Hugging Face: the text model, the vision and audio
//! encoders and the tables they read, in one download with one progress total. The file
//! list comes from the repo's tree API, so every file inside each `.mlpackage` is fetched
//! whatever the package holds; anything else in the repo (README, …) is skipped.
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
/// The image encoder (`vision_70` / `vision_140` / `vision_280`).
pub(crate) const VISION_PACKAGE: &str = "EmbeddingGemma2Vision.mlpackage";
/// The audio encoder.
pub(crate) const AUDIO_PACKAGE: &str = "EmbeddingGemma2Audio.mlpackage";
/// Every package a download fetches.
pub(crate) const PACKAGES: &[&str] = &[TEXT_PACKAGE, VISION_PACKAGE, AUDIO_PACKAGE];
/// The vision encoder's patch position tables.
pub(crate) const POSITION_TABLE: &str = "position_embeddings.f16";
/// Top-level files the model reads besides the packages.
const TOP_FILES: &[&str] = &[
    "config.json",
    "tokenizer.json",
    "embeddings.bf16",
    POSITION_TABLE,
];

/// Where a package is kept compiled: `X.mlpackage` → `X.mlmodelc`.
pub(crate) fn compiled_name(package: &str) -> String {
    format!("{}.mlmodelc", package.trim_end_matches(".mlpackage"))
}

/// The package a repo file belongs to.
fn package_of(path: &str) -> Option<&'static str> {
    PACKAGES.iter().copied().find(|p| {
        path.strip_prefix(p)
            .is_some_and(|rest| rest.starts_with('/'))
    })
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

/// The files the model needs, from `{hf_base}/api/models/{REPO}/tree/main`.
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
    let files: Vec<RemoteFile> = tree
        .into_iter()
        .filter(|e| e.kind == "file")
        .filter(|e| TOP_FILES.contains(&e.path.as_str()) || package_of(&e.path).is_some())
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
    for package in PACKAGES {
        if !files.iter().any(|f| package_of(&f.path) == Some(package)) {
            return Err(EmbedError::Download(format!("{REPO} has no {package}")));
        }
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
    let mut fetched: Vec<&str> = Vec::new();
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
        if let Some(package) = package_of(&file.path)
            && !fetched.contains(&package)
        {
            fetched.push(package);
        }
    }

    // New package files need a new compile.
    for package in fetched {
        remove_if_exists(&dir.join(compiled_name(package))).await?;
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
        && PACKAGES
            .iter()
            .all(|p| files.iter().any(|f| package_of(&f.path) == Some(p)))
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

    /// A fake Hugging Face repo: the tree listing plus small files: every model part, and
    /// files a download must skip.
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
            (
                "EmbeddingGemma2Audio.mlpackage/Data/com.apple.CoreML/weights/weight.bin",
                vec![4u8; 40_000],
            ),
            (
                "EmbeddingGemma2Vision.mlpackage/Manifest.json",
                b"{}".to_vec(),
            ),
            (
                "EmbeddingGemma2Vision.mlpackage/Data/com.apple.CoreML/weights/weight.bin",
                vec![5u8; 30_000],
            ),
            ("position_embeddings.f16", vec![3u8; 900]),
            // Not part of the model: skipped, even though its name starts like a package's.
            ("EmbeddingGemma2Text.mlpackage.zip", vec![9u8; 10]),
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

    fn wanted(path: &str) -> bool {
        TOP_FILES.contains(&path) || package_of(path).is_some()
    }

    /// Bytes of every model part: text, vision, audio and the tables.
    fn model_bytes() -> u64 {
        fake_files()
            .iter()
            .filter(|(p, _)| wanted(p))
            .map(|(_, b)| b.len() as u64)
            .sum()
    }

    #[test]
    fn files_belong_to_packages_by_folder() {
        assert_eq!(
            package_of("EmbeddingGemma2Vision.mlpackage/Manifest.json"),
            Some(VISION_PACKAGE)
        );
        assert_eq!(package_of("EmbeddingGemma2Text.mlpackage.zip"), None);
        assert_eq!(package_of("README.md"), None);
        assert_eq!(
            compiled_name(AUDIO_PACKAGE),
            "EmbeddingGemma2Audio.mlmodelc"
        );
        assert_eq!(compiled_name(TEXT_PACKAGE), "EmbeddingGemma2Text.mlmodelc");
    }

    #[tokio::test]
    async fn downloads_every_model_part_with_one_progress_total() {
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
            model_bytes()
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
        assert_eq!(seen.first(), Some(&(0, model_bytes())));
        assert_eq!(seen.last(), Some(&(model_bytes(), model_bytes())));
        assert!(seen.windows(2).all(|w| w[0].0 <= w[1].0));

        assert!(files.is_complete());
        let parts = fake_files().iter().filter(|(p, _)| wanted(p)).count();
        assert_eq!(parts, 11);
        assert_eq!(repo.fetches.load(Ordering::SeqCst), parts);
        for (path, bytes) in fake_files() {
            let on_disk = std::fs::read(files.dir.join(&path)).ok();
            if wanted(&path) {
                assert_eq!(on_disk.as_ref(), Some(&bytes), "{path}");
            } else {
                assert_eq!(on_disk, None, "{path} should be skipped");
            }
        }
        assert!(files.size_on_disk() >= model_bytes());

        // A missing file makes it incomplete; downloading again fetches only that file,
        // and drops the compiled copy of the package it belongs to (only that one).
        for package in PACKAGES {
            std::fs::create_dir_all(files.dir.join(compiled_name(package))).unwrap();
        }
        let weights = "EmbeddingGemma2Vision.mlpackage/Data/com.apple.CoreML/weights/weight.bin";
        std::fs::remove_file(files.dir.join(weights)).unwrap();
        assert!(!files.is_complete());
        files.download(&client, &base, &|_, _| {}).await.unwrap();
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), parts + 1);
        assert!(!files.dir.join(compiled_name(VISION_PACKAGE)).exists());
        assert!(files.dir.join(compiled_name(TEXT_PACKAGE)).exists());
        assert!(files.dir.join(compiled_name(AUDIO_PACKAGE)).exists());

        // A text-only download from before the encoders were added isn't complete.
        std::fs::remove_dir_all(files.dir.join(AUDIO_PACKAGE)).unwrap();
        assert!(!files.is_complete());
        files.download(&client, &base, &|_, _| {}).await.unwrap();
        assert!(files.is_complete());
        assert_eq!(repo.fetches.load(Ordering::SeqCst), parts + 3);

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

        // Every encoder is required too.
        let files: Vec<_> = fake_files()
            .into_iter()
            .filter(|(p, _)| package_of(p) != Some(AUDIO_PACKAGE))
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
        assert!(err.to_string().contains(AUDIO_PACKAGE), "{err}");
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
