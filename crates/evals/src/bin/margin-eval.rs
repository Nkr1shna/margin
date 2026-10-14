//! The opt-in eval runner (`docs/agent-rag-architecture.md` §3.4). Never run in CI:
//! every command that reaches a model or the real embedder checks `live::guard_env`
//! first. The commands themselves live in `margin_evals::runner` and are tested there
//! with fakes; this file only wires the real models, embedder and Keychain to them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use margin_evals::cli::{Command, Suite, USAGE, check_service, parse};
use margin_evals::corpus::{EXTRACTED_FILE, Extraction, documents};
use margin_evals::gates::{BASELINE_FILE, Baseline};
use margin_evals::golden::evals_dir;
use margin_evals::judge::Judge;
use margin_evals::live;
use margin_evals::recorded::{EMBEDDINGS_FILE, Recording, RecordingVectors, Vectors};
use margin_evals::runner::{self, Env, Paths};
use margin_evals::store::{EvalStore, default_data_dir};
use margin_evals::traces::{self, TraceFilter};
use margin_service::secrets::KeychainStore;

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Err(e) = run(&args).await {
        eprintln!("margin-eval: {e:#}");
        std::process::exit(1);
    }
}

async fn run(args: &[String]) -> anyhow::Result<()> {
    let cli = parse(args).map_err(|e| anyhow::anyhow!("{e}\n\n{USAGE}"))?;
    if cli.command == Command::Help {
        print!("{USAGE}");
        return Ok(());
    }
    if cli.command.is_live() {
        live::guard_env()?;
    }
    // run, export and report --service read the service: dev only (docs/plan-deploy.md §5).
    if let Some(url) = &cli.service_url {
        let v = check_service(&reqwest::Client::new(), url).await?;
        eprintln!("service {url}: {} {} ({})", v.environment, v.version, v.sha);
    }
    let evals = cli.evals.clone().unwrap_or_else(evals_dir);
    let paths = Paths::same(&evals);
    let data = cli.data.clone().unwrap_or_else(default_data_dir);
    let store = EvalStore::open(&data).await?;
    let token = || {
        cli.token
            .clone()
            .or_else(|| std::env::var("MARGIN_DEVICE_TOKEN").ok())
            .context(
                "reading the service's traces needs a device token: --token or MARGIN_DEVICE_TOKEN",
            )
    };
    match &cli.command {
        Command::Runs => {
            for r in store.runs().await? {
                println!(
                    "{}  {:<9} {:<10} {}{}",
                    r.id,
                    r.suite,
                    r.variant,
                    r.started_at.format("%Y-%m-%d %H:%M"),
                    if r.valid { "" } else { "  INVALID" }
                );
            }
        }
        Command::Compare {
            a,
            b,
            allow_embedder_change,
        } => {
            print!(
                "{}",
                runner::compare_runs(&store, a, b, *allow_embedder_change)
                    .await?
                    .markdown()
            );
        }
        Command::Export { filter, out } => {
            let url = cli
                .service_url
                .as_deref()
                .context("export needs --service-url")?;
            let rows = traces::fetch(&reqwest::Client::new(), url, &token()?, filter).await?;
            traces::write_jsonl(out, &rows)?;
            println!("{} trace rows → {}", rows.len(), out.display());
        }
        Command::Report {
            run,
            traces: file,
            service,
        } => {
            let prices = prices(&evals)?;
            let rows = match (file, service) {
                (Some(f), _) => Some(traces::read_file(f, &TraceFilter::default())?),
                (None, true) => {
                    let url = cli
                        .service_url
                        .as_deref()
                        .context("report --service needs --service-url")?;
                    Some(
                        traces::fetch(
                            &reqwest::Client::new(),
                            url,
                            &token()?,
                            &TraceFilter::default(),
                        )
                        .await?,
                    )
                }
                (None, false) => None,
            };
            let eff = rows.map(|r| traces::efficiency(&r, &prices));
            let (md, html) =
                runner::write_report(&paths, &store, run.as_deref(), eff.as_ref()).await?;
            println!("{}\n{}", md.display(), html.display());
        }
        live_cmd => live_command(live_cmd, &paths, &store).await?,
    }
    Ok(())
}

fn prices(evals: &Path) -> anyhow::Result<margin_evals::Prices> {
    Ok(toml::from_str(&std::fs::read_to_string(
        evals.join("prices.toml"),
    )?)?)
}

fn media_dir(store_data: &Path) -> PathBuf {
    store_data.join("media")
}

fn judges(models: &live::Models) -> Vec<Box<dyn Judge>> {
    models
        .small
        .iter()
        .map(|m| Box::new(margin_evals::ai_judge::AiJudge { model: m.clone() }) as Box<dyn Judge>)
        .collect()
}

fn generator(models: &live::Models) -> Box<dyn Judge> {
    Box::new(margin_evals::ai_judge::AiJudge {
        model: models.writer.clone(),
    })
}

fn need_judges(js: &[Box<dyn Judge>]) -> anyhow::Result<()> {
    anyhow::ensure!(
        !js.is_empty(),
        "judging needs a small model: set one in evals/runner.toml"
    );
    Ok(())
}

#[cfg(target_os = "macos")]
async fn live_command(cmd: &Command, paths: &Paths, store: &EvalStore) -> anyhow::Result<()> {
    let cfg = live::RunnerConfig::load(&paths.evals.join("runner.toml"))?;
    let data = default_data_dir();
    let media = media_dir(&data);
    let corpus = paths.corpus();
    let docs = documents(&corpus)?;
    let embedder: &'static live::LiveVectors =
        Box::leak(Box::new(live::LiveVectors::load(&corpus, &media)?));
    let extraction = match Extraction::load(&paths.evals.join(EXTRACTED_FILE)) {
        Ok(e) if !matches!(cmd, Command::RecordEmbeddings) => {
            // Page renders for the image encoder come from a fresh read either way.
            live::extract_corpus(&corpus, &docs, &media, "pdfkit+vision")?;
            e
        }
        _ => live::extract_corpus(
            &corpus,
            &docs,
            &media,
            &format!("pdfkit+vision on macOS {}", os_version()),
        )?,
    };
    let seed = Recording::load(&paths.evals.join(EMBEDDINGS_FILE))
        .ok()
        .filter(|r| r.version == embedder.version())
        .unwrap_or_else(|| Recording::for_version(embedder.version()));
    // Served from the recording where it has the vector, embedded live otherwise.
    let vectors: &'static RecordingVectors<'static> =
        Box::leak(Box::new(RecordingVectors::new(embedder, seed)?));
    let env = Env {
        paths: paths.clone(),
        vectors: if matches!(cmd, Command::RecordEmbeddings) {
            embedder
        } else {
            vectors
        },
        extraction,
        register: &|_| {},
        pack_budget: &margin_service::retrieval::pack_budget,
    };
    let needs_models = !matches!(
        cmd,
        Command::RecordEmbeddings
            | Command::Run {
                suite: Suite::Retrieval,
                ..
            }
    );
    let models = if needs_models {
        Some(live::models(&cfg, &KeychainStore)?)
    } else {
        None
    };
    match cmd {
        Command::RecordEmbeddings => {
            let s = runner::record_embeddings(&env).await?;
            println!(
                "{} vectors recorded ({} embedded now) for chunkers {:?} → {}",
                s.vectors,
                s.embedded,
                s.chunkers,
                paths.evals.join(EMBEDDINGS_FILE).display()
            );
        }
        Command::RecordGenerations { variant } => {
            let m = models.as_ref().unwrap();
            let (run, scores) = runner::run_courses(
                &env,
                variant,
                Some(m.writer.as_ref()),
                &[],
                cfg.window_tokens,
                store,
            )
            .await?;
            println!("{}: {} lessons recorded", run.id, scores.len());
        }
        Command::SynthQuestions {
            variant,
            per,
            limit,
        } => {
            let m = models.as_ref().unwrap();
            let js = judges(m);
            need_judges(&js)?;
            // The grader on the other provider than the writer when both are configured.
            let grader = js
                .iter()
                .find(|j| !j.id().starts_with(m.writer.provider()))
                .unwrap_or(&js[0]);
            let writer = generator(m);
            let (run, s) = runner::synth_questions(
                &env,
                variant,
                writer.as_ref(),
                grader.as_ref(),
                *per,
                *limit,
                store,
            )
            .await?;
            println!(
                "{}: {} generated, {} failed checks, grades {:?}, {} kept",
                run.id, s.generated, s.failed_checks, s.grades, s.kept
            );
        }
        Command::GradeQuestions { prune } => {
            let js = judges(models.as_ref().unwrap());
            need_judges(&js)?;
            let refs: Vec<&dyn Judge> = js.iter().map(|j| j.as_ref()).collect();
            let run = runner::grade_questions(&env, &refs, *prune, store).await?;
            println!(
                "{}{}",
                run.id,
                if run.valid {
                    ""
                } else {
                    "  INVALID: a grader failed its canaries"
                }
            );
        }
        Command::Run {
            suite,
            variant,
            write_baseline,
            judge,
        } => match suite {
            Suite::Retrieval => {
                let (run, r) = runner::run_retrieval(&env, variant, store, *write_baseline).await?;
                println!(
                    "{}: recall@5 {:.1}, nDCG@10 {:.1}, negatives FP {:.1} %, p95 {:.1} ms",
                    run.id,
                    r.overall.recall_at_5,
                    r.overall.ndcg_at_10,
                    r.negative_fp_rate,
                    r.latency_p95_ms
                );
                if *write_baseline {
                    let b = Baseline::load(&paths.out.join(BASELINE_FILE))?;
                    println!(
                        "baseline written for {} ({})",
                        b.variant, b.embedder_version
                    );
                }
            }
            Suite::Courses => {
                let m = models.as_ref().unwrap();
                let js = if *judge { judges(m) } else { Vec::new() };
                if *judge {
                    need_judges(&js)?;
                }
                let refs: Vec<&dyn Judge> = js.iter().map(|j| j.as_ref()).collect();
                let (run, scores) = runner::run_courses(
                    &env,
                    variant,
                    Some(m.writer.as_ref()),
                    &refs,
                    cfg.window_tokens,
                    store,
                )
                .await?;
                println!(
                    "{}: {} lessons{}",
                    run.id,
                    scores.len(),
                    if run.valid { "" } else { "  INVALID" }
                );
            }
            Suite::Agent => {
                let m = models.as_ref().unwrap();
                let (client, model_id) = m.chat.clone().context("the agent suite runs on the OpenAI Responses API: no openai-api-key in the Keychain")?;
                let js = if *judge { judges(m) } else { Vec::new() };
                if *judge {
                    need_judges(&js)?;
                }
                let refs: Vec<&dyn Judge> = js.iter().map(|j| j.as_ref()).collect();
                let (pcfg, _) = paths.variant(variant)?;
                let index = Arc::new(
                    margin_evals::index::EvalIndex::build(
                        margin_service::db::Store::memory().await?,
                        &pcfg,
                        |_| {},
                        vectors,
                        &corpus,
                        &docs,
                        &env.extraction,
                    )
                    .await?,
                );
                let (run, results) = runner::run_agent(
                    &env,
                    variant,
                    index,
                    &client,
                    &model_id,
                    &refs,
                    margin_agent::RetryPolicy::default(),
                    store,
                )
                .await?;
                println!(
                    "{}: {} tasks, {} finished",
                    run.id,
                    results.len(),
                    results.iter().filter(|r| r.finished).count()
                );
            }
        },
        Command::Judge { dir, against } => {
            let js = judges(models.as_ref().unwrap());
            need_judges(&js)?;
            let refs: Vec<&dyn Judge> = js.iter().map(|j| j.as_ref()).collect();
            let (pcfg, _) = paths.variant("baseline")?;
            let dir = dir.clone().unwrap_or_else(|| {
                paths
                    .evals
                    .join(margin_evals::courses::GENERATIONS_DIR)
                    .join(&pcfg.prompts)
            });
            let run =
                runner::judge_generations(&env, &dir, against.as_deref(), &refs, store).await?;
            println!(
                "{}{}",
                run.id,
                if run.valid {
                    ""
                } else {
                    "  INVALID: a judge failed its canaries"
                }
            );
        }
        _ => unreachable!("not a live command"),
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
async fn live_command(_: &Command, _: &Paths, _: &EvalStore) -> anyhow::Result<()> {
    anyhow::bail!(
        "this command needs the service Mac: the real embedder and PDF/OCR readers are macOS only"
    )
}

#[cfg(target_os = "macos")]
fn os_version() -> String {
    std::process::Command::new("sw_vers")
        .arg("-productVersion")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "?".into())
}
