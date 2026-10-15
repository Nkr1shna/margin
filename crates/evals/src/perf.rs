//! `margin-eval perf --service-url …` (`docs/phase5-brief.md` D8): ingests
//! `evals/corpus` through the service under test, runs the retrieval golden set over
//! `POST /api/context/search`, runs one agent turn and one lesson, reads the trace rows
//! back and prints each budget of `evals/budgets.toml` as met or missed. Reported, not
//! gated: machines differ.

use std::path::Path;

/// The budgets, under the evals folder.
pub const BUDGETS_FILE: &str = "budgets.toml";

/// Measures the service at `url` against `evals/budgets.toml`.
pub async fn run(
    _http: &reqwest::Client,
    _url: &str,
    _token: &str,
    _evals: &Path,
) -> anyhow::Result<()> {
    unimplemented!("P5, 5-ops")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::golden::evals_dir;

    /// `budgets.toml` parses and holds the D8 table.
    #[test]
    fn budgets_parse() {
        let text = std::fs::read_to_string(evals_dir().join(BUDGETS_FILE)).unwrap();
        let t: toml::Table = toml::from_str(&text).unwrap();
        for key in [
            "ingest",
            "retrieval",
            "context_search",
            "agent_step",
            "cache",
            "title",
            "memory",
            "folder_sync",
        ] {
            assert!(t.contains_key(key), "no [{key}]");
        }
    }
}
