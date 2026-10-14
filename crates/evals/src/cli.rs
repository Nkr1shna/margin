//! `margin-eval`'s command line, parsed (the binary runs it).

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use margin_shared::context::TraceKind;

use crate::traces::TraceFilter;

pub const USAGE: &str = "\
margin-eval: Margin's opt-in eval runner. It calls real models with the owner's keys
from the service Mac's Keychain and loads the real embedder, so it never runs in CI
(it refuses when CI is set); CI runs `cargo test -p margin-evals` on recorded fixtures.
It keeps its own database (--data, default ~/Library/Application Support/Margin/evals)
and never opens the service's.

Usage: margin-eval <command> [options]

Commands:
  record-embeddings     Read the corpus with PDFKit/Vision and record the extraction and
                        the real EmbeddingGemma vectors of every corpus chunk, picture and
                        golden query (evals/recorded/{extracted.json,embeddings.bin})
  record-generations [--variant NAME]
                        Write and record every golden lesson with the writing model
                        (evals/recorded/generations/<prompt version>/<model>/)
  synth-questions [--variant NAME] [--per N] [--limit N]
                        Generate serious questions per section, check their spans, grade
                        them on the other provider, append those graded >= 2 to
                        evals/golden/retrieval.jsonl and every candidate to
                        evals/golden/synth-log.jsonl
  grade-questions [--prune]
                        Grade every golden question with every grader: distribution,
                        inter-grader kappa, canaries; --prune drops generated rows now < 2
  run --suite retrieval|courses|agent [--variant NAME] [--write-baseline] [--no-judge]
                        Run a suite with evals/variants/NAME.toml (default baseline)
  judge [--dir PATH] [--against PATH]
                        Judge recorded generations (default: the baseline's prompt
                        version), with canaries; --against compares pairwise
  compare RUN_A RUN_B [--allow-embedder-change]
                        Per-metric deltas with paired bootstrap 95% intervals
  report [RUN] [--traces FILE | --service]
                        Write evals/reports/RUN.md and RUN.html (latest run by default),
                        with efficiency from exported traces or the running service
  export [--kind K] [--from T] [--to T] [--trace-id ID] [--out FILE]
                        Export the service's trace rows over GET /api/traces to JSONL
  runs                  List the runs in the runner's database

Options for every command:
  --evals DIR           The evals folder (default: the repository's evals/)
  --data DIR            The runner's data folder
  --token TOKEN         Device token for the service (or MARGIN_DEVICE_TOKEN)
";

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    RecordEmbeddings,
    RecordGenerations {
        variant: String,
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
    Runs,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Suite {
    Retrieval,
    Courses,
    Agent,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Cli {
    pub command: Command,
    pub evals: Option<PathBuf>,
    pub data: Option<PathBuf>,
    pub token: Option<String>,
}

impl Command {
    /// Whether it calls a real model or loads the real embedder.
    pub fn is_live(&self) -> bool {
        !matches!(
            self,
            Command::Compare { .. }
                | Command::Report { .. }
                | Command::Export { .. }
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
    let variant = take("variant").unwrap_or_else(|| "baseline".into());
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
    let help = flag(take("help"));
    let command = match pos.first().map(String::as_str) {
        _ if help => Command::Help,
        None | Some("help") => Command::Help,
        Some("record-embeddings") => Command::RecordEmbeddings,
        Some("record-generations") => Command::RecordGenerations { variant },
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
                Some(other) => bail!("no suite \"{other}\" (retrieval, courses, agent)"),
                None => bail!("run needs --suite retrieval|courses|agent"),
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
        Some("runs") => Command::Runs,
        Some(other) => bail!("no command \"{other}\""),
    };
    if let Some(k) = opts.keys().next()
        && command != Command::Help
    {
        bail!("--{k} doesn't apply here");
    }
    Ok(Cli {
        command,
        evals,
        data,
        token,
    })
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
            p("run --suite retrieval --variant bm25 --write-baseline")
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
            p("run --suite=courses --no-judge").unwrap().command,
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
                allow_embedder_change: true
            }
        );
        let c = p("export --kind model_call --from 2026-10-01T00:00:00Z --out x.jsonl --data /tmp/d --token t").unwrap();
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
            p("report r1 --service").unwrap().command,
            Command::Report {
                run: Some("r1".into()),
                traces: None,
                service: true
            }
        );
        assert!(p("record-embeddings").unwrap().command.is_live());
        assert!(!p("runs").unwrap().command.is_live());
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
}
