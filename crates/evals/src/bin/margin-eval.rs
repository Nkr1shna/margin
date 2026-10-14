//! The opt-in eval runner. Its sub-commands are built by R1-evals; this prints what
//! they will be.

const USAGE: &str = "\
margin-eval: Margin's opt-in eval runner (never run in CI)

Usage: margin-eval <command> [options]

Commands (built in R1; see evals/README.md):
  record-embeddings     Record the real EmbeddingGemma vectors of the corpus and the
                        golden queries into evals/recorded/embeddings.bin
  record-generations    Record model outputs for the golden courses
  synth-questions       Generate serious retrieval questions per section
  grade-questions       Grade generated questions for seriousness (0-3)
  run --suite <retrieval|courses|agent> --variant <name>
                        Run a suite with evals/variants/<name>.toml
  judge                 Judge recorded generations against the rubrics
  compare <run-a> <run-b>
                        Per-metric deltas with paired bootstrap intervals
  report                Write evals/reports/<run>.md and a single-file HTML report
  export                Export the service's trace rows to JSONL
";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        None | Some("-h" | "--help" | "help") => print!("{USAGE}"),
        Some(other) => {
            eprintln!("margin-eval: \"{other}\" isn't built yet.\n\n{USAGE}");
            std::process::exit(2);
        }
    }
}
