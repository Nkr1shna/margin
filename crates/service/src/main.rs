//! `margin-service setup` | `margin-service serve [--allow-unsandboxed-render]` |
//! `margin-service download-model [--embedder NAME]` | `margin-service scrub-restored` |
//! `margin-service version`

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use margin_service::config::{self, Config};
use margin_service::db::Store;
use margin_service::engines::model_download;
use margin_service::engines::{AiGenerator, EmbedTools, VideoRenderer};
use margin_service::server::{Options, Service, shutdown_signal};
use margin_service::{restore, version};

const USAGE: &str = "\
Usage:
  margin-service setup                               Write the config and store the GitHub client secret
  margin-service serve [--allow-unsandboxed-render]  Run the service
  margin-service download-model [--embedder NAME]    Download the embedding model (the configured
                                                     embedder, or NAME: coreml, onnx-q8,
                                                     onnx-fp32, onnx-fp16) without the app,
                                                     and on Linux the speech model
  margin-service scrub-restored                      After a prod -> dev restore, with the
                                                     service stopped: delete devices and traces
                                                     (a dev config only)
  margin-service version                             Print the build's version and commit
";

/// Where the config is read from on this OS, for the help text.
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
        Some("download-model") => match model_download::parse_args(&args[1..]) {
            Ok(embedder) => download_model(embedder),
            Err(e) => {
                eprintln!("{e}\n\n{usage}");
                return ExitCode::from(2);
            }
        },
        Some("version" | "--version") if args.len() == 1 => {
            println!(
                "margin-service {} ({})",
                env!("CARGO_PKG_VERSION"),
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
        #[cfg(any(target_os = "linux", feature = "linux-readers"))]
        margin_embed::set_tools_dir(config.data_dir.join("tools"));
        let source_tools = Arc::new(EmbedTools::new(&config.models_dir()));
        let store = Store::open(&config.db_dir()).await?;
        let listener = tokio::net::TcpListener::bind(&config.bind)
            .await
            .with_context(|| format!("couldn't listen on {}", config.bind))?;
        tracing::info!(
            "Margin service {} ({}) listening on {} (public URL {}, environment {}, embedder {}, secrets in {})",
            env!("CARGO_PKG_VERSION"),
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
        service.serve(listener, shutdown_signal()).await
    })
}

/// Downloads the embedding model into the data folder, and the speech model where this
/// build transcribes with one (Linux), printing progress: what the Settings buttons do,
/// for a headless box (the provisioning script runs it).
fn download_model(embedder: Option<margin_embed::Backend>) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let config = Config::load(&config::config_path())?;
        let files = model_download::files_for(&config, embedder)?;
        let report = |line: &str| eprintln!("{line}");
        let tools = EmbedTools::new(&config.models_dir());
        model_download::run(&tools, &files, margin_embed::HF_BASE, &report).await?;
        if let Some(speech) = margin_service::engines::SourceTools::speech_model(&tools) {
            model_download::run_speech(speech, margin_embed::HF_BASE, &report).await?;
        }
        Ok(())
    })
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
        let store = Store::open(&config.db_dir())
            .await
            .context("couldn't open the database; stop the service first")?;
        for done in restore::scrub(&store).await? {
            println!("deleted {} {} rows", done.rows, done.table);
        }
        Ok(())
    })
}
