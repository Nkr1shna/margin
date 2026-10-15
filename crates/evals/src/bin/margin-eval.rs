//! The opt-in eval runner (`docs/agent-rag-architecture.md` §3.4). Never run in CI:
//! every command that reaches a model or the real embedder checks `live::guard_env`
//! first. The commands themselves live in `margin_evals::runner` and are tested there
//! with fakes; this file only wires the real models, embedder and secret store to them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context;
use margin_evals::cli::{Command, Suite, USAGE, check_service, parse};
use margin_evals::corpus::{EXTRACTED_FILE, Extraction, documents};
use margin_evals::gates::{BASELINE_FILE, Baseline};
use margin_evals::golden::evals_dir;
use margin_evals::harvest;
use margin_evals::judge::Judge;
use margin_evals::live;
use margin_evals::recorded::{EMBEDDINGS_FILE, Recording, RecordingVectors, Vectors};
use margin_evals::runner::{self, Env, Paths};
use margin_evals::store::{EvalStore, default_data_dir};
use margin_evals::traces::{self, TraceFilter};

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
            allow_provider_change,
        } => {
            let allow = margin_evals::compare::Allow {
                embedder_change: *allow_embedder_change,
                provider_change: *allow_provider_change,
            };
            print!(
                "{}",
                runner::compare_runs(&store, a, b, allow).await?.markdown()
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
        Command::Harvest { since, out } => {
            let url = cli
                .service_url
                .as_deref()
                .context("harvest needs --service-url")?;
            let http = reqwest::Client::new();
            let token = token()?;
            let service = harvest::Service {
                http: &http,
                url,
                token: &token,
            };
            let found = harvest::harvest(&service, *since).await?;
            let out = out
                .clone()
                .unwrap_or_else(|| evals.join(harvest::CANDIDATES_FILE));
            let added = harvest::append_candidates(&out, &found)?;
            for (actor, n) in harvest::by_actor(&found) {
                println!("{actor}: {n} searches with reads");
            }
            println!(
                "{} candidates, {added} new → {} (promote the serious ones into {})",
                found.len(),
                out.display(),
                evals.join(harvest::DOGFOOD_FILE).display()
            );
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
        Command::Alerts => {
            let url = cli
                .service_url
                .as_deref()
                .context("alerts needs --service-url")?;
            margin_evals::alerts::run(&reqwest::Client::new(), url, &token()?).await?;
        }
        Command::Perf => {
            let url = cli
                .service_url
                .as_deref()
                .context("perf needs --service-url")?;
            margin_evals::perf::run(&reqwest::Client::new(), url, &token()?, &evals).await?;
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

async fn live_command(cmd: &Command, paths: &Paths, store: &EvalStore) -> anyhow::Result<()> {
    let cfg = live::RunnerConfig::load(&paths.evals.join("runner.toml"))?;
    // The dogfood suite reads the repository's docs/ (next to evals/) as its corpus.
    let dogfood;
    let paths = if matches!(
        cmd,
        Command::Run {
            suite: Suite::Dogfood,
            ..
        }
    ) {
        let docs = paths.evals.join("../docs");
        anyhow::ensure!(docs.is_dir(), "no docs/ next to {}", paths.evals.display());
        dogfood = paths.dogfood(&docs);
        &dogfood
    } else {
        paths
    };
    // The backend: the environment's, the variant's `[embedder]`, or the service's.
    let service = live::service_config()?;
    let variant_embedder = match cmd {
        Command::Run { variant, .. }
        | Command::RecordGenerations { variant }
        | Command::SynthQuestions { variant, .. } => Some(paths.variant(variant)?.0.embedder.name),
        _ => None,
    };
    let backend = live::live_backend(
        &|k| std::env::var(k).ok(),
        variant_embedder.as_deref(),
        &service.embedder,
    )?;
    eprintln!(
        "embedder: {} ({})",
        backend.label(),
        backend.embedder_version()
    );
    let reader = live::reader_name();
    let recording_file = paths.evals.join(live::recording_file(backend));
    let extraction_file = paths.evals.join(live::extraction_file(reader));
    let data = default_data_dir();
    let media = media_dir(&data);
    let corpus = paths.corpus();
    let docs = documents(&corpus)?;
    let embedder: &'static live::LiveVectors = Box::leak(Box::new(live::LiveVectors::load(
        &corpus, &media, &service, backend,
    )?));
    let extraction = match Extraction::load(&extraction_file) {
        Ok(e) if !matches!(cmd, Command::RecordEmbeddings) => {
            // Page renders for the image encoder come from a fresh read either way.
            live::extract_corpus(&corpus, &docs, &media, reader)?;
            e
        }
        _ => live::extract_corpus(
            &corpus,
            &docs,
            &media,
            &format!("{reader} on {}", live::os_version()),
        )?,
    };
    let seed = Recording::load(&recording_file)
        .ok()
        .filter(|r| r.version == embedder.version())
        .unwrap_or_else(|| Recording::for_version(embedder.version()));
    // Served from the recording where it has the vector, embedded live otherwise.
    let vectors: &'static RecordingVectors<'static> =
        Box::leak(Box::new(RecordingVectors::new(embedder, seed)?));
    // `record_embeddings` writes `EMBEDDINGS_FILE` and `EXTRACTED_FILE` under `out`: for
    // another backend or reader it writes into a staging folder (seeded with that
    // backend's recording) whose files are then copied to their own names, so the Core
    // ML recording and the Mac extraction are never overwritten.
    let staged = recording_file != paths.evals.join(EMBEDDINGS_FILE)
        || extraction_file != paths.evals.join(EXTRACTED_FILE);
    let stage = data.join("record-staging").join(backend.dir_name());
    let mut env_paths = paths.clone();
    if matches!(cmd, Command::RecordEmbeddings) && staged {
        let _ = std::fs::remove_dir_all(&stage);
        std::fs::create_dir_all(stage.join("recorded"))?;
        if recording_file.is_file() {
            std::fs::copy(&recording_file, stage.join(EMBEDDINGS_FILE))?;
        }
        env_paths.out = stage.clone();
    }
    let env = Env {
        paths: env_paths,
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
                suite: Suite::Retrieval | Suite::Dogfood,
                ..
            }
    );
    let models = if needs_models {
        Some(live::models(&cfg, &*live::secret_store(&service)?)?)
    } else {
        None
    };
    match cmd {
        Command::RecordEmbeddings => {
            let s = runner::record_embeddings(&env).await?;
            if staged {
                for (from, to) in [
                    (stage.join(EMBEDDINGS_FILE), &recording_file),
                    (stage.join(EXTRACTED_FILE), &extraction_file),
                ] {
                    if let Some(dir) = to.parent() {
                        std::fs::create_dir_all(dir)?;
                    }
                    std::fs::copy(&from, to).with_context(|| {
                        format!("couldn't copy {} to {}", from.display(), to.display())
                    })?;
                }
                let _ = std::fs::remove_dir_all(&stage);
            }
            println!(
                "{} vectors recorded ({} embedded now) for chunkers {:?} → {} (extraction → {})",
                s.vectors,
                s.embedded,
                s.chunkers,
                recording_file.display(),
                extraction_file.display()
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
            Suite::Dogfood => {
                anyhow::ensure!(
                    !*write_baseline,
                    "the dogfood suite has no baseline (CI never runs it)"
                );
                let (run, r) = runner::run_dogfood(&env, variant, store).await?;
                println!(
                    "{}: dogfood recall@5 {:.1}, nDCG@10 {:.1} over {} questions",
                    run.id,
                    r.overall.recall_at_5,
                    r.overall.ndcg_at_10,
                    r.queries.len()
                );
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
            Suite::Injection => {
                margin_evals::injection::run(&env, variant, models.as_ref().unwrap(), store)
                    .await?;
            }
            Suite::Agent => {
                let m = models.as_ref().unwrap();
                let (client, model_id) = m.chat.clone().context("the agent suite runs on the OpenAI Responses API: no openai-api-key in the service's secret store")?;
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
