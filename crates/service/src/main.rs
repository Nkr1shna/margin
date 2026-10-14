//! `margin-service setup` | `margin-service serve [--allow-unsandboxed-render]`

use std::process::ExitCode;
use std::sync::Arc;

use anyhow::Context;
use margin_service::config::{self, Config};
use margin_service::db::Store;
use margin_service::engines::{AiGenerator, EmbedTools, VideoRenderer};
use margin_service::secrets::KeychainStore;
use margin_service::server::{Options, Service, shutdown_signal};

const USAGE: &str = "\
Usage:
  margin-service setup                               Write the config and store the GitHub client secret
  margin-service serve [--allow-unsandboxed-render]  Run the service

Config: $MARGIN_CONFIG, or ~/Library/Application Support/Margin/service.toml";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        Some("setup") if args.len() == 1 => {
            config::run_setup(&config::config_path(), &KeychainStore)
        }
        Some("serve") => {
            let mut allow_unsandboxed = false;
            for a in &args[1..] {
                match a.as_str() {
                    "--allow-unsandboxed-render" => allow_unsandboxed = true,
                    other => {
                        eprintln!("unknown option {other}\n\n{USAGE}");
                        return ExitCode::from(2);
                    }
                }
            }
            serve(allow_unsandboxed)
        }
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        _ => {
            eprintln!("{USAGE}");
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
        if allow_unsandboxed_render {
            tracing::warn!(
                "--allow-unsandboxed-render: scene code will run WITHOUT the macOS sandbox"
            );
        }
        let store = Store::open(&config.db_dir()).await?;
        let listener = tokio::net::TcpListener::bind(&config.bind)
            .await
            .with_context(|| format!("couldn't listen on {}", config.bind))?;
        tracing::info!(
            "Margin service listening on {} (public URL {})",
            config.bind,
            config.public_url
        );
        let service = Service::start(Options {
            config,
            store,
            secrets: Arc::new(KeychainStore),
            generator: Arc::new(AiGenerator),
            renderer: Arc::new(VideoRenderer),
            source_tools: Arc::new(EmbedTools),
            hf_base: margin_embed::HF_BASE.into(),
            allow_unsandboxed_render,
        })
        .await?;
        service.serve(listener, shutdown_signal()).await
    })
}
