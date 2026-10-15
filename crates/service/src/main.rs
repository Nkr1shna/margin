//! `margin-service setup` | `margin-service serve [--allow-unsandboxed-render]` |
//! `margin-service download-model [--embedder NAME] [--reranker NAME]` |
//! `margin-service scrub-restored` | `margin-service check-data <dir>` |
//! `margin-service backup [--label NAME]` |
//! `margin-service export --person <id> --out <dir>` |
//! `margin-service version`

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use margin_service::config::{self, Config};
use margin_service::engines::model_download;
use margin_service::engines::{AiGenerator, EmbedTools, VideoRenderer};
use margin_service::server::{Options, Service, shutdown_signal};
use margin_service::{export, restore, version};

const USAGE: &str = "\
Usage:
  margin-service setup                               Write the config and store the GitHub client secret
  margin-service serve [--allow-unsandboxed-render]  Run the service
  margin-service download-model [--embedder NAME]    Download the embedding model (the configured
                                                     embedder, or NAME: onnx-q8,
                                                     onnx-fp32, onnx-fp16) without the app,
                                                     and the speech model
  margin-service download-model --reranker NAME      Download a reranker model instead
                                                     (ms-marco-minilm-l6-v2,
                                                     bge-reranker-base) for the
                                                     onnx-crossencoder reranker
                                                     (bge-reranker-base needs ~1.3 GB
                                                     of memory loaded)
  margin-service scrub-restored                      After a prod -> dev restore, with the
                                                     service stopped: delete devices and traces
                                                     (a dev config only)
  margin-service check-data <dir>                    Check a data folder (a backup) on a copy:
                                                     tables, migrations, source files
  margin-service backup [--label NAME]               Back up the data folder now
  margin-service export --person <id> --out <dir>    With the service stopped: export one
                                                     person's library from a copy (logged,
                                                     shown in the Audit view)
  margin-service version                             Print the build's version and commit
";

/// How long a stopping service waits for queued audit rows to be written.
const AUDIT_FLUSH_ON_STOP: std::time::Duration = std::time::Duration::from_secs(10);

/// Where the config is read from, for the help text.
fn config_help() -> String {
    format!(
        "Config: $MARGIN_CONFIG, or {}",
        config::default_config_path().display()
    )
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let usage = format!("{USAGE}\n{}", config_help());
    let result = match args.first().map(String::as_str) {
        Some("setup") if args.len() == 1 => config::run_setup(&config::config_path()),
        Some("scrub-restored") if args.len() == 1 => scrub_restored(),
        Some("check-data") if args.len() == 2 => check_data(std::path::Path::new(&args[1])),
        Some("backup") => match &args[1..] {
            [] => backup(None),
            [flag, label] if flag == "--label" => backup(Some(label)),
            _ => {
                eprintln!("{usage}");
                return ExitCode::from(2);
            }
        },
        Some("export") => match export::parse_args(&args[1..]) {
            Ok((person, out)) => export_person(&person, &out),
            Err(e) => {
                eprintln!("{e}\n\n{usage}");
                return ExitCode::from(2);
            }
        },
        Some("download-model") => {
            let parsed = split_reranker(&args[1..]).and_then(|(reranker, rest)| {
                let embedder = model_download::parse_args(&rest)?;
                Ok((embedder, reranker))
            });
            match parsed {
                Ok((embedder, reranker)) => download_model(embedder, reranker),
                Err(e) => {
                    eprintln!("{e}\n\n{usage}");
                    return ExitCode::from(2);
                }
            }
        }
        Some("version" | "--version") if args.len() == 1 => {
            println!(
                "margin-service {} ({})",
                version::VERSION,
                version::BUILD_SHA
            );
            return ExitCode::SUCCESS;
        }
        Some("serve") => {
            let mut allow_unsandboxed = false;
            for a in &args[1..] {
                match a.as_str() {
                    "--allow-unsandboxed-render" => allow_unsandboxed = true,
                    other => {
                        eprintln!("unknown option {other}\n\n{usage}");
                        return ExitCode::from(2);
                    }
                }
            }
            serve(allow_unsandboxed)
        }
        Some("-h" | "--help" | "help") => {
            println!("{usage}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{usage}");
            return ExitCode::from(2);
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn serve(allow_unsandboxed_render: bool) -> anyhow::Result<()> {
    version::mark_started();
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("info,surrealdb=warn,surrealkv=warn")
            }),
        )
        .init();
    // SurrealDB's embedded engine wants a generous stack.
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(10 * 1024 * 1024)
        .build()?;
    runtime.block_on(async move {
        let path = config::config_path();
        let config = Config::load(&path)?;
        config.validate_for_serving()?;
        if let Some(warning) = version::memory_warning(version::physical_memory_bytes()) {
            tracing::warn!("{warning}");
        }
        if allow_unsandboxed_render {
            tracing::warn!(
                "--allow-unsandboxed-render: scene code will run WITHOUT the render sandbox"
            );
        }
        let secrets = config.secret_store()?;
        // Tools a user installed without root (the pdfium library) are looked for here.
        margin_embed::set_tools_dir(config.data_dir.join("tools"));
        let source_tools = Arc::new(EmbedTools::new(&config.models_dir()));
        // Refuses data a newer build wrote; backs up data an older one wrote before
        // this build migrates it (`restore::open_guarded`).
        let (store, backup) = restore::open_guarded(&config).await?;
        if let Some(dir) = backup {
            tracing::info!(
                "backed up the data before migrating it to schema {}: {}",
                margin_service::db::SCHEMA_VERSION,
                dir.display()
            );
        }
        let listener = tokio::net::TcpListener::bind(&config.bind)
            .await
            .with_context(|| format!("couldn't listen on {}", config.bind))?;
        tracing::info!(
            "Margin service {} ({}) listening on {} (public URL {}, environment {}, embedder {}, secrets in {})",
            version::VERSION,
            version::BUILD_SHA,
            config.bind,
            config.public_url,
            config.environment,
            config.embedder,
            secrets.location()
        );
        let service = Service::start(Options {
            config,
            store,
            secrets,
            generator: Arc::new(AiGenerator),
            renderer: Arc::new(VideoRenderer),
            source_tools,
            hf_base: margin_embed::HF_BASE.into(),
            allow_unsandboxed_render,
        })
        .await?;
        let state = service.state.clone();
        let served = service.serve(listener, shutdown_signal()).await;
        // Audit rows are written off a queue: every row recorded before the stop is in
        // the store before the process exits (traces may be cut short; audit rows not).
        if !margin_service::audit::flush(&state, AUDIT_FLUSH_ON_STOP).await {
            tracing::error!(
                "audit rows were still being written when the service stopped; the log above \
                 names any that weren't"
            );
        }
        served
    })
}

/// Takes `--reranker NAME` (or `--reranker=NAME`) out of `download-model`'s arguments:
/// the cross-encoder to download (`margin_embed::RerankerModel::name`), and the rest.
fn split_reranker(
    args: &[String],
) -> Result<(Option<margin_embed::RerankerModel>, Vec<String>), String> {
    let mut reranker = None;
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let name = match a.as_str() {
            "--reranker" => it
                .next()
                .cloned()
                .ok_or_else(|| "--reranker needs a name".to_string())?,
            other => match other.strip_prefix("--reranker=") {
                Some(v) => v.to_string(),
                None => {
                    rest.push(a.clone());
                    continue;
                }
            },
        };
        let model = margin_embed::RerankerModel::from_name(&name).ok_or_else(|| {
            let known: Vec<&str> = margin_embed::RerankerModel::ALL
                .iter()
                .map(|m| m.name())
                .collect();
            format!(
                "there is no reranker model named {name} (known: {})",
                known.join(", ")
            )
        })?;
        reranker = Some(model);
    }
    Ok((reranker, rest))
}

/// Downloads the embedding model and the speech model into the data folder, printing
/// progress: what the Settings buttons do, for a headless box (the provisioning script
/// runs it). With `--reranker NAME` it
/// downloads that cross-encoder instead (and the embedder too when `--embedder` names
/// one).
fn download_model(
    embedder: Option<margin_embed::Backend>,
    reranker: Option<margin_embed::RerankerModel>,
) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let config = Config::load(&config::config_path())?;
        let report = |line: &str| eprintln!("{line}");
        if reranker.is_none() || embedder.is_some() {
            let files = model_download::files_for(&config, embedder)?;
            let tools = EmbedTools::new(&config.models_dir());
            model_download::run(&tools, &files, margin_embed::HF_BASE, &report).await?;
            if let Some(speech) = margin_service::engines::SourceTools::speech_model(&tools) {
                model_download::run_speech(speech, margin_embed::HF_BASE, &report).await?;
            }
        }
        if let Some(model) = reranker {
            let files = margin_embed::RerankerFiles::new(&config.models_dir(), model);
            download_reranker(&files, margin_embed::HF_BASE, &report).await?;
        }
        Ok(())
    })
}

/// Downloads a reranker model unless it is complete, reporting progress lines (at most
/// every two seconds) through `report`.
async fn download_reranker(
    files: &margin_embed::RerankerFiles,
    hf_base: &str,
    report: &(dyn Fn(&str) + Send + Sync),
) -> anyhow::Result<()> {
    let name = files.model.name();
    let mb = |bytes: u64| format!("{:.1} MB", bytes as f64 / 1_000_000.0);
    if files.is_complete() {
        report(&format!(
            "The reranker model {name} is already downloaded in {} ({}).",
            files.dir.display(),
            mb(files.size_on_disk())
        ));
        return Ok(());
    }
    let http = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .read_timeout(std::time::Duration::from_secs(120))
        .build()?;
    report(&format!(
        "Downloading the reranker model {name} from {} (revision {}) into {}…",
        files.model.repo(),
        &files.model.revision()[..12],
        files.dir.display()
    ));
    let last = std::sync::Mutex::new(None::<std::time::Instant>);
    let progress = |received: u64, total: u64| {
        let mut last = last.lock().unwrap();
        let due = last.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2));
        if due || received == total {
            *last = Some(std::time::Instant::now());
            report(&format!("  {} of {}", mb(received), mb(total)));
        }
    };
    files
        .download(&http, hf_base, &progress)
        .await
        .with_context(|| format!("couldn't download the reranker model {name}"))?;
    anyhow::ensure!(
        files.is_complete(),
        "the download of the reranker model {name} finished but isn't complete in {}",
        files.dir.display()
    );
    report(&format!(
        "The reranker model {name} is downloaded ({}). Pick the rerank-xenc variant in Settings → AI → Pipeline to use it.",
        mb(files.size_on_disk())
    ));
    Ok(())
}

/// Opens the database directly (the service must be stopped: SurrealKV takes a lock)
/// and deletes what dev must not inherit from prod (`restore.rs`).
fn scrub_restored() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(10 * 1024 * 1024)
        .build()?;
    runtime.block_on(async {
        let config = Config::load(&config::config_path())?;
        restore::check_environment(&config)?;
        let (store, _) = restore::open_guarded(&config)
            .await
            .context("couldn't open the database; stop the service first")?;
        for done in restore::scrub(&store).await? {
            println!("deleted {} {} rows", done.rows, done.table);
        }
        Ok(())
    })
}

/// Checks a data folder on a copy and prints the report (`count <table> <n>` lines,
/// the problems, a last one-line verdict); fails on any problem. Needs no config.
fn check_data(dir: &std::path::Path) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(10 * 1024 * 1024)
        .build()?;
    runtime.block_on(async {
        let check = restore::check_data(dir).await?;
        print!("{}", check.report(dir));
        anyhow::ensure!(
            check.problems.is_empty(),
            "{} has {} problem(s)",
            dir.display(),
            check.problems.len()
        );
        Ok(())
    })
}

/// Exports one person's library from the configured data folder (L2 D7).
fn export_person(person: &str, out: &std::path::Path) -> anyhow::Result<()> {
    let config = Config::load(&config::config_path())?;
    export::run(&config.data_dir, person, out)
}

/// Backs up the configured data folder (the service must be stopped) and checks the
/// copy; prints `BACKUP=<folder>` last, like `backup-margin.sh`.
fn backup(label: Option<&str>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(10 * 1024 * 1024)
        .build()?;
    runtime.block_on(async {
        let config = Config::load(&config::config_path())?;
        let dir = restore::backup(&config, label).await?;
        println!("Backed up and checked {}.", config.data_dir.display());
        println!("BACKUP={}", dir.display());
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(a: &[&str]) -> Vec<String> {
        a.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn download_model_takes_a_reranker_beside_the_embedder() {
        use margin_embed::RerankerModel;
        assert_eq!(split_reranker(&[]).unwrap(), (None, vec![]));
        assert_eq!(
            split_reranker(&args(&["--reranker", "ms-marco-minilm-l6-v2"])).unwrap(),
            (Some(RerankerModel::MsMarcoMiniLmL6), vec![])
        );
        assert_eq!(
            split_reranker(&args(&[
                "--embedder",
                "onnx-q8",
                "--reranker=bge-reranker-base"
            ]))
            .unwrap(),
            (
                Some(RerankerModel::BgeRerankerBase),
                args(&["--embedder", "onnx-q8"])
            )
        );
        assert!(split_reranker(&args(&["--reranker"])).is_err());
        let err = split_reranker(&args(&["--reranker", "nope"])).unwrap_err();
        assert!(err.contains("ms-marco-minilm-l6-v2"), "{err}");
    }
}
