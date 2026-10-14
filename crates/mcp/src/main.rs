//! The `margin-mcp` command. See `docs/plan-dogfood.md` §2 and `USAGE` below.

use std::io::Read;
use std::process::ExitCode;

use anyhow::{Context, bail};
use margin_mcp::client::Client;
use margin_mcp::config::{Keychain, config_path, require_token, service_url};
use margin_mcp::ingest::{self, CLAUDE_CODE};
use margin_mcp::{login, serve};

const USAGE: &str = "\
margin-mcp: Margin as Claude Code's knowledge base

USAGE:
  margin-mcp serve                     MCP server on stdin/stdout (for .mcp.json)
  margin-mcp login                     sign in with GitHub; the token goes to the Keychain
  margin-mcp ingest --name <name> [--kind decision|handoff|note|text|code]
                    [--agent <name>] (<file> | -)
                                       add one document (default agent: claude-code)
  margin-mcp ingest-dir [--dry-run] <path>...
                                       add the tracked *.md under the paths as agent:repo,
                                       named <path>@<short sha>; older versions are replaced
  margin-mcp purge --actor agent:<name> [--dry-run]
                                       delete every document that agent wrote

OPTIONS:
  --url <url>   the Margin service (default: `url` in ~/.config/margin/mcp.toml)
  -h, --help    this text
";

#[derive(Default)]
struct Args {
    command: String,
    url: Option<String>,
    name: Option<String>,
    kind: Option<String>,
    agent: Option<String>,
    actor: Option<String>,
    dry_run: bool,
    positional: Vec<String>,
}

fn parse(mut argv: impl Iterator<Item = String>) -> anyhow::Result<Option<Args>> {
    let mut args = Args::default();
    while let Some(arg) = argv.next() {
        let mut value = |flag: &str| argv.next().with_context(|| format!("{flag} needs a value"));
        match arg.as_str() {
            "-h" | "--help" | "help" => return Ok(None),
            "--url" => args.url = Some(value("--url")?),
            "--name" => args.name = Some(value("--name")?),
            "--kind" => args.kind = Some(value("--kind")?),
            "--agent" => args.agent = Some(value("--agent")?),
            "--actor" => args.actor = Some(value("--actor")?),
            "--dry-run" | "-n" => args.dry_run = true,
            "-" => args.positional.push(arg),
            flag if flag.starts_with('-') => bail!("unknown option {flag}"),
            _ if args.command.is_empty() => args.command = arg,
            _ => args.positional.push(arg),
        }
    }
    if args.command.is_empty() {
        return Ok(None);
    }
    Ok(Some(args))
}

fn say(line: &str) {
    eprintln!("{line}");
}

async fn signed_in_client(url: Option<&str>) -> anyhow::Result<Client> {
    let url = service_url(url, &config_path()?)?;
    let token = require_token(&Keychain, &url)?;
    Client::new(&url, Some(token))
}

async fn run(args: Args) -> anyhow::Result<()> {
    match args.command.as_str() {
        "serve" => {
            let client = signed_in_client(args.url.as_deref()).await?;
            serve::run(client, tokio::io::stdin(), tokio::io::stdout()).await
        }
        "login" => {
            let url = service_url(args.url.as_deref(), &config_path()?)?;
            let client = Client::new(&url, None)?;
            login::login(
                &client,
                &login::device_name(),
                &Keychain,
                |url| open::that(url).map_err(Into::into),
                &mut say,
                login::POLL_INTERVAL,
            )
            .await
            .map(|_| ())
        }
        "ingest" => {
            let name = args.name.as_deref().context("ingest needs --name")?;
            let [source] = args.positional.as_slice() else {
                bail!("ingest takes one file, or - for stdin");
            };
            let text = if source == "-" {
                let mut text = String::new();
                std::io::stdin()
                    .read_to_string(&mut text)
                    .context("couldn't read stdin")?;
                text
            } else {
                std::fs::read_to_string(source)
                    .with_context(|| format!("couldn't read {source}"))?
            };
            let agent = args.agent.as_deref().unwrap_or(CLAUDE_CODE);
            let client = signed_in_client(args.url.as_deref()).await?;
            let doc = ingest::ingest_text(&client, agent, name, args.kind.as_deref(), text).await?;
            say(&format!("added {} ({}) as {}", doc.name, doc.id, doc.actor));
            Ok(())
        }
        "ingest-dir" => {
            let client = signed_in_client(args.url.as_deref()).await?;
            let cwd = std::env::current_dir().context("no current directory")?;
            ingest::ingest_dir(&client, &cwd, &args.positional, args.dry_run, &mut say)
                .await
                .map(|_| ())
        }
        "purge" => {
            let actor = args
                .actor
                .as_deref()
                .context("purge needs --actor agent:<name>")?;
            let client = signed_in_client(args.url.as_deref()).await?;
            ingest::purge(&client, actor, args.dry_run, &mut say)
                .await
                .map(|_| ())
        }
        other => bail!("unknown command {other:?}\n\n{USAGE}"),
    }
}

#[tokio::main]
async fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(Some(args)) => args,
        Ok(None) => {
            print!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("margin-mcp: {e:#}\n\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("margin-mcp: {e:#}");
            ExitCode::FAILURE
        }
    }
}
