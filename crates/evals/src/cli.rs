//! `margin-eval`'s command line, parsed (the binary runs it).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use margin_service::version::VersionInfo;
use margin_shared::context::TraceKind;
use margin_shared::routes;

use crate::traces::TraceFilter;

/// The prod service's host: `margin-eval` never reads from or runs against it
/// (`docs/plan-deploy.md` §5). Evals and harvesting point at dev.
pub const PROD_HOST: &str = "margin.krishnanelloore.com";
/// What the usage suggests instead.
pub const DEV_SERVICE_URL: &str = "https://devmargin.krishnanelloore.com";

pub const USAGE: &str = "\
margin-eval: Margin's opt-in eval runner. It calls real models with the owner's keys
from the service's secret store (the Keychain on a Mac, the secrets file on Linux) and
loads the real embedder, so it never runs in CI (it refuses when CI is set); CI runs
`cargo test -p margin-evals` on recorded fixtures. It keeps its own database (--data,
default `evals` in the service's support folder: ~/Library/Application Support/Margin
on a Mac, /var/lib/margin on Linux) and never opens the service's.

Usage: margin-eval <command> [options]

Commands:
  record-embeddings     Read the corpus with this OS's readers (PDFKit/Vision on a Mac,
                        pdfium/tesseract/ffmpeg on Linux) and record the extraction and
                        the real EmbeddingGemma vectors of every corpus chunk, picture and
                        golden query (evals/recorded/{extracted.json,embeddings.bin})
  record-generations [--variant NAME]
                        Write and record every golden lesson with the writing model
                        (evals/recorded/generations/<prompt version>/<model>/; another
                        variant's under <prompt version>+<variant>/)
  record-rewrites [--variant NAME]
                        Run the variant's query rewriter (default rewrite-small) on the
                        small-tasks model once per golden query and lesson plan and record
                        each answer (evals/recorded/rewrites.jsonl); CI replays them. Then
                        run record-embeddings, which embeds the recorded rewrites too
  record-reranks [--variant NAME]
                        Run the variant's listwise reranker (default rerank-small) on the
                        small-tasks model once per golden query and lesson plan over its
                        fused candidates and record each answer
                        (evals/recorded/reranks.jsonl); CI replays them. For a variant
                        that also rewrites, record-rewrites and record-embeddings first
  synth-questions [--variant NAME] [--per N] [--limit N]
                        Generate serious questions per section, check their spans, grade
                        them on the other provider, append those graded >= 2 to
                        evals/golden/retrieval.jsonl and every candidate to
                        evals/golden/synth-log.jsonl
  grade-questions [--prune]
                        Grade every golden question with every grader: distribution,
                        inter-grader kappa, canaries; --prune drops generated rows now < 2
  run --suite retrieval|courses|agent|dogfood --service-url URL [--variant NAME]
      [--write-baseline] [--no-judge]
                        Run a suite with evals/variants/NAME.toml (default baseline)
                        beside the dev service at URL; dogfood runs the retrieval suite
                        over the repository's docs/ with evals/golden/dogfood.jsonl
  harvest --service-url URL [--since T] [--out FILE]
                        Turn agents' context-API searches and the passages they read
                        after them (trace rows with via \"mcp\") into candidate questions,
                        appended to evals/golden/candidates.jsonl (or FILE); promote the
                        serious ones into evals/golden/dogfood.jsonl
  judge [--dir PATH] [--against PATH]
                        Judge recorded generations (default: the baseline's prompt
                        version), with canaries; --against compares pairwise
  compare RUN_A RUN_B [--allow-embedder-change] [--allow-provider-change]
      [--allow-prompt-change]
                        Per-metric deltas with paired bootstrap 95% intervals; runs
                        whose models came from different providers (OpenAI, ChatGPT,
                        Claude) are refused without --allow-provider-change, runs that
                        ran one prompt under two versions without --allow-prompt-change
  prompts [--write-lock]
                        Every prompt's version and constant-block hash, and whether
                        evals/prompts.lock agrees; --write-lock rewrites the lock (with a
                        bumped version, in the same change as the prompt)
  report [RUN] [--traces FILE | --service --service-url URL]
                        Write evals/reports/RUN.md and RUN.html (latest run by default),
                        with efficiency from exported traces or the service at URL
  export --service-url URL [--kind K] [--from T] [--to T] [--trace-id ID] [--out FILE]
                        Export the service's trace rows over GET /api/traces to JSONL
  runs                  List the runs in the runner's database

Options for every command:
  --evals DIR           The evals folder (default: the repository's evals/)
  --data DIR            The runner's data folder
  --token TOKEN         Device token for the service (or MARGIN_DEVICE_TOKEN)

--service-url is the dev service (https://devmargin.krishnanelloore.com) or a local
one; margin-eval refuses the prod host, and any service whose GET /api/version
reports environment \"prod\".
";

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    RecordEmbeddings,
    RecordGenerations {
        variant: String,
    },
    /// R2: the rewriter's answers on the small-tasks model.
    RecordRewrites {
        variant: String,
    },
    /// R2: the listwise reranker's answers on the small-tasks model.
    RecordReranks {
        variant: String,
    },
    /// R2: the prompt catalog against `evals/prompts.lock`.
    Prompts {
        write_lock: bool,
    },
    SynthQuestions {
        variant: String,
        per: u32,
        limit: usize,
    },
    GradeQuestions {
        prune: bool,
    },
    Run {
        suite: Suite,
        variant: String,
        write_baseline: bool,
        judge: bool,
    },
    Judge {
        dir: Option<PathBuf>,
        against: Option<PathBuf>,
    },
    Compare {
        a: String,
        b: String,
        allow_embedder_change: bool,
        allow_provider_change: bool,
        allow_prompt_change: bool,
    },
    Report {
        run: Option<String>,
        traces: Option<PathBuf>,
        service: bool,
    },
    Export {
        filter: TraceFilter,
        out: PathBuf,
    },
    /// `docs/plan-dogfood.md` §6. `out` defaults to `evals/golden/candidates.jsonl`.
    Harvest {
        since: Option<DateTime<Utc>>,
        out: Option<PathBuf>,
    },
    Runs,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    Retrieval,
    Courses,
    Agent,
    /// The retrieval suite over `docs/` with the questions agents really asked.
    Dogfood,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cli {
    pub command: Command,
    pub evals: Option<PathBuf>,
    pub data: Option<PathBuf>,
    pub token: Option<String>,
    /// `--service-url`, checked by `check_service_url` (never the prod host).
    pub service_url: Option<String>,
}

impl Command {
    /// Whether it reads the service, so `--service-url` is required: `run`, `export`,
    /// `harvest` and `report --service`.
    pub fn needs_service_url(&self) -> bool {
        matches!(
            self,
            Command::Run { .. }
                | Command::Export { .. }
                | Command::Harvest { .. }
                | Command::Report { service: true, .. }
        )
    }

    /// Whether it calls a real model or loads the real embedder.
    pub fn is_live(&self) -> bool {
        !matches!(
            self,
            Command::Compare { .. }
                | Command::Prompts { .. }
                | Command::Report { .. }
                | Command::Export { .. }
                | Command::Harvest { .. }
                | Command::Runs
                | Command::Help
        )
    }
}

const FLAGS: &[&str] = &[
    "--help",
    "--prune",
    "--write-baseline",
    "--no-judge",
    "--allow-embedder-change",
    "--allow-provider-change",
    "--allow-prompt-change",
    "--write-lock",
    "--service",
];

pub fn parse(args: &[String]) -> anyhow::Result<Cli> {
    let mut pos = Vec::new();
    let mut opts: BTreeMap<String, String> = BTreeMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(name) = a.strip_prefix("--") {
            if FLAGS.contains(&a.as_str()) {
                opts.insert(name.into(), "true".into());
            } else if let Some((k, v)) = name.split_once('=') {
                opts.insert(k.into(), v.into());
            } else {
                let v = args
                    .get(i + 1)
                    .with_context(|| format!("{a} needs a value"))?;
                opts.insert(name.into(), v.clone());
                i += 1;
            }
        } else if a == "-h" {
            opts.insert("help".into(), "true".into());
        } else {
            pos.push(a.clone());
        }
        i += 1;
    }
    let mut take = |k: &str| opts.remove(k);
    let variant_given = take("variant");
    let variant = variant_given.clone().unwrap_or_else(|| "baseline".into());
    let flag = |o: Option<String>| o.is_some();
    let time = |o: Option<String>| -> anyhow::Result<Option<DateTime<Utc>>> {
        o.map(|t| {
            DateTime::parse_from_rfc3339(&t)
                .map(|d| d.with_timezone(&Utc))
                .with_context(|| format!("{t} isn't an RFC 3339 time"))
        })
        .transpose()
    };
    let evals = take("evals").map(PathBuf::from);
    let data = take("data").map(PathBuf::from);
    let token = take("token");
    let service_url = take("service-url");
    let help = flag(take("help"));
    let command = match pos.first().map(String::as_str) {
        _ if help => Command::Help,
        None | Some("help") => Command::Help,
        Some("record-embeddings") => Command::RecordEmbeddings,
        Some("record-generations") => Command::RecordGenerations { variant },
        Some("record-rewrites") => Command::RecordRewrites {
            variant: variant_given.unwrap_or_else(|| "rewrite-small".into()),
        },
        Some("record-reranks") => Command::RecordReranks {
            variant: variant_given.unwrap_or_else(|| "rerank-small".into()),
        },
        Some("prompts") => Command::Prompts {
            write_lock: flag(take("write-lock")),
        },
        Some("synth-questions") => Command::SynthQuestions {
            variant,
            per: take("per").map(|v| v.parse()).transpose()?.unwrap_or(4),
            limit: take("limit")
                .map(|v| v.parse())
                .transpose()?
                .unwrap_or(usize::MAX),
        },
        Some("grade-questions") => Command::GradeQuestions {
            prune: flag(take("prune")),
        },
        Some("run") => Command::Run {
            suite: match take("suite").as_deref() {
                Some("retrieval") => Suite::Retrieval,
                Some("courses") => Suite::Courses,
                Some("agent") => Suite::Agent,
                Some("dogfood") => Suite::Dogfood,
                Some(other) => {
                    bail!("no suite \"{other}\" (retrieval, courses, agent, dogfood)")
                }
                None => bail!("run needs --suite retrieval|courses|agent|dogfood"),
            },
            variant,
            write_baseline: flag(take("write-baseline")),
            judge: !flag(take("no-judge")),
        },
        Some("judge") => Command::Judge {
            dir: take("dir").map(PathBuf::from),
            against: take("against").map(PathBuf::from),
        },
        Some("compare") => match (pos.get(1), pos.get(2)) {
            (Some(a), Some(b)) => Command::Compare {
                a: a.clone(),
                b: b.clone(),
                allow_embedder_change: flag(take("allow-embedder-change")),
                allow_provider_change: flag(take("allow-provider-change")),
                allow_prompt_change: flag(take("allow-prompt-change")),
            },
            _ => bail!("compare needs two run ids (see `margin-eval runs`)"),
        },
        Some("report") => Command::Report {
            run: pos.get(1).cloned(),
            traces: take("traces").map(PathBuf::from),
            service: flag(take("service")),
        },
        Some("export") => Command::Export {
            filter: TraceFilter {
                kind: take("kind")
                    .map(|k| {
                        TraceKind::ALL
                            .into_iter()
                            .find(|t| t.as_str() == k)
                            .with_context(|| format!("no trace kind \"{k}\""))
                    })
                    .transpose()?,
                from: time(take("from"))?,
                to: time(take("to"))?,
                trace_id: take("trace-id"),
            },
            out: take("out")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("traces.jsonl")),
        },
        Some("harvest") => Command::Harvest {
            since: time(take("since"))?,
            out: take("out").map(PathBuf::from),
        },
        Some("runs") => Command::Runs,
        Some(other) => bail!("no command \"{other}\""),
    };
    if let Some(k) = opts.keys().next()
        && command != Command::Help
    {
        bail!("--{k} doesn't apply here");
    }
    let service_url = match (command.needs_service_url(), service_url) {
        (true, Some(url)) => Some(check_service_url(&url)?),
        (true, None) => bail!(
            "this command reads the service: add --service-url (the dev service, {DEV_SERVICE_URL})"
        ),
        (false, Some(_)) if command != Command::Help => {
            bail!("--service-url doesn't apply here")
        }
        (false, _) => None,
    };
    Ok(Cli {
        command,
        evals,
        data,
        token,
        service_url,
    })
}

/// Trims `url` and its trailing slashes, and refuses anything that isn't http(s) or
/// whose host is the prod service's.
pub fn check_service_url(url: &str) -> anyhow::Result<String> {
    let url = url.trim().trim_end_matches('/').to_string();
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        bail!("--service-url must start with https:// or http://");
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let host_port = authority.rsplit('@').next().unwrap_or_default();
    let host = match host_port.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or_default(),
        None => host_port.split(':').next().unwrap_or_default(),
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    if host.is_empty() {
        bail!("--service-url {url} has no host");
    }
    if host == PROD_HOST {
        bail!(
            "--service-url {url} is the prod service; evals and harvesting never read prod \
             (use the dev service, {DEV_SERVICE_URL})"
        );
    }
    Ok(url)
}

/// Asks the service which deploy it is (`GET /api/version`) and refuses prod, so a
/// local address that is really the owner's library (environment "prod", the default
/// of a config written before deploys) is refused too.
pub async fn check_service(client: &reqwest::Client, url: &str) -> anyhow::Result<VersionInfo> {
    let res = client
        .get(format!("{url}{}", routes::VERSION))
        .timeout(std::time::Duration::from_secs(15))
        .send()
        .await
        .with_context(|| format!("couldn't reach {url}"))?;
    if !res.status().is_success() {
        bail!(
            "{url} doesn't say which environment it is (GET {} → {}); deploy a build with it",
            routes::VERSION,
            res.status()
        );
    }
    let v: VersionInfo = res
        .json()
        .await
        .with_context(|| format!("{url}{} isn't a version", routes::VERSION))?;
    if v.environment != "dev" {
        bail!(
            "{url} is a \"{}\" service; evals and harvesting only read dev ({DEV_SERVICE_URL})",
            v.environment
        );
    }
    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(s: &str) -> anyhow::Result<Cli> {
        parse(&s.split_whitespace().map(String::from).collect::<Vec<_>>())
    }

    #[test]
    fn parses_every_command() {
        assert_eq!(p("").unwrap().command, Command::Help);
        assert_eq!(
            p("run --suite retrieval --help").unwrap().command,
            Command::Help
        );
        assert_eq!(
            p("run --suite retrieval --variant bm25 --write-baseline --service-url http://127.0.0.1:8787")
                .unwrap()
                .command,
            Command::Run {
                suite: Suite::Retrieval,
                variant: "bm25".into(),
                write_baseline: true,
                judge: true
            }
        );
        assert_eq!(
            p("run --suite=courses --no-judge --service-url=http://127.0.0.1:8787")
                .unwrap()
                .command,
            Command::Run {
                suite: Suite::Courses,
                variant: "baseline".into(),
                write_baseline: false,
                judge: false
            }
        );
        assert_eq!(
            p("synth-questions --per 3 --limit 10").unwrap().command,
            Command::SynthQuestions {
                variant: "baseline".into(),
                per: 3,
                limit: 10
            }
        );
        assert_eq!(
            p("compare a b --allow-embedder-change").unwrap().command,
            Command::Compare {
                a: "a".into(),
                b: "b".into(),
                allow_embedder_change: true,
                allow_provider_change: false,
                allow_prompt_change: false
            }
        );
        assert_eq!(
            p("compare a b --allow-provider-change").unwrap().command,
            Command::Compare {
                a: "a".into(),
                b: "b".into(),
                allow_embedder_change: false,
                allow_provider_change: true,
                allow_prompt_change: false
            }
        );
        let c = p("export --kind model_call --from 2026-10-01T00:00:00Z --out x.jsonl --data /tmp/d --token t --service-url http://127.0.0.1:8787").unwrap();
        assert_eq!(c.service_url.as_deref(), Some("http://127.0.0.1:8787"));
        assert_eq!(c.data, Some(PathBuf::from("/tmp/d")));
        assert_eq!(c.token.as_deref(), Some("t"));
        match c.command {
            Command::Export { filter, out } => {
                assert_eq!(filter.kind, Some(TraceKind::ModelCall));
                assert!(filter.from.is_some());
                assert_eq!(out, PathBuf::from("x.jsonl"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            p("report r1 --service --service-url http://127.0.0.1:8787")
                .unwrap()
                .command,
            Command::Report {
                run: Some("r1".into()),
                traces: None,
                service: true
            }
        );
        assert_eq!(
            p("run --suite retrieval --service-url https://devmargin.krishnanelloore.com/")
                .unwrap()
                .service_url
                .as_deref(),
            Some("https://devmargin.krishnanelloore.com")
        );
        assert!(p("record-embeddings").unwrap().command.is_live());
        // R2: the small-stage recordings (default variants), the prompt lock, and
        // compare across prompt versions.
        assert_eq!(
            p("record-rewrites").unwrap().command,
            Command::RecordRewrites {
                variant: "rewrite-small".into()
            }
        );
        assert_eq!(
            p("record-reranks --variant r2-full").unwrap().command,
            Command::RecordReranks {
                variant: "r2-full".into()
            }
        );
        assert!(p("record-reranks").unwrap().command.is_live());
        assert_eq!(
            p("prompts --write-lock").unwrap().command,
            Command::Prompts { write_lock: true }
        );
        assert!(!p("prompts").unwrap().command.is_live());
        assert_eq!(
            p("compare a b --allow-prompt-change").unwrap().command,
            Command::Compare {
                a: "a".into(),
                b: "b".into(),
                allow_embedder_change: false,
                allow_provider_change: false,
                allow_prompt_change: true
            }
        );
        assert!(!p("runs").unwrap().command.is_live());
        // The dogfood feed (plan-dogfood §6).
        assert_eq!(
            p("run --suite dogfood --service-url http://127.0.0.1:8787")
                .unwrap()
                .command,
            Command::Run {
                suite: Suite::Dogfood,
                variant: "baseline".into(),
                write_baseline: false,
                judge: true
            }
        );
        let h = p("harvest --since 2026-10-01T00:00:00Z --out c.jsonl --service-url http://127.0.0.1:8787").unwrap();
        assert!(!h.command.is_live());
        match h.command {
            Command::Harvest { since, out } => {
                assert!(since.is_some());
                assert_eq!(out, Some(PathBuf::from("c.jsonl")));
            }
            other => panic!("{other:?}"),
        }
        // Harvesting reads the service, never prod.
        assert!(
            p("harvest")
                .unwrap_err()
                .to_string()
                .contains("--service-url")
        );
        assert!(
            p("harvest --service-url https://margin.krishnanelloore.com")
                .unwrap_err()
                .to_string()
                .contains("prod")
        );
    }

    #[test]
    fn rejects_mistakes() {
        assert!(p("run").is_err());
        assert!(p("run --suite nope").is_err());
        assert!(p("compare a").is_err());
        assert!(p("frobnicate").is_err());
        assert!(
            p("runs --prune")
                .unwrap_err()
                .to_string()
                .contains("--prune doesn't apply")
        );
        assert!(p("export --kind bogus").is_err());
        assert!(p("export --from yesterday").is_err());
        assert!(p("run --suite retrieval --variant").is_err());
    }

    async fn fake_service(environment: Option<&'static str>) -> String {
        use axum::routing::get;
        let app = axum::Router::new().route(
            routes::VERSION,
            get(move || async move {
                match environment {
                    Some(env) => axum::Json(serde_json::json!({
                        "version": "0.1.0", "sha": "abc", "environment": env,
                        "started_at": "2026-10-14T00:00:00Z", "rss_bytes": 1
                    }))
                    .into_response(),
                    None => axum::http::StatusCode::NOT_FOUND.into_response(),
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        format!("http://{addr}")
    }

    use axum::response::IntoResponse;

    #[tokio::test]
    async fn the_service_must_say_it_is_dev() {
        let client = reqwest::Client::new();
        let dev = fake_service(Some("dev")).await;
        assert_eq!(check_service(&client, &dev).await.unwrap().sha, "abc");
        let prod = fake_service(Some("prod")).await;
        let err = check_service(&client, &prod).await.unwrap_err().to_string();
        assert!(err.contains("\"prod\" service"), "{err}");
        let old = fake_service(None).await;
        let err = check_service(&client, &old).await.unwrap_err().to_string();
        assert!(err.contains("doesn't say"), "{err}");
    }

    #[test]
    fn service_url_is_required_and_never_prod() {
        for cmd in ["run --suite retrieval", "export", "report --service"] {
            let err = p(cmd).unwrap_err().to_string();
            assert!(err.contains("--service-url"), "{cmd}: {err}");
        }
        // Commands that don't read the service neither need nor take it.
        assert!(p("report r1").unwrap().service_url.is_none());
        assert!(
            p("runs --service-url http://127.0.0.1:8787")
                .unwrap_err()
                .to_string()
                .contains("doesn't apply")
        );
        for prod in [
            "https://margin.krishnanelloore.com",
            "https://Margin.KrishnaNelloore.com/",
            "https://margin.krishnanelloore.com./api",
            "http://margin.krishnanelloore.com:8787",
            "https://user@margin.krishnanelloore.com",
        ] {
            let err = check_service_url(prod).unwrap_err().to_string();
            assert!(err.contains("prod"), "{prod}: {err}");
            assert!(
                p(&format!("export --service-url {prod}")).is_err(),
                "{prod}"
            );
        }
        assert!(check_service_url("devmargin.krishnanelloore.com").is_err());
        assert!(check_service_url("https://").is_err());
        assert_eq!(
            check_service_url(" https://devmargin.krishnanelloore.com// ").unwrap(),
            "https://devmargin.krishnanelloore.com"
        );
        assert_eq!(
            check_service_url("http://[::1]:8787").unwrap(),
            "http://[::1]:8787"
        );
    }
}
