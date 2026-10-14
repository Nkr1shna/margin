//! The runner's own database (§3.4): a SurrealDB file in its own folder
//! (`~/Library/Application Support/Margin/evals/db` by default, never the service's
//! database), with `eval_run` records and `eval_result:[run_id, at, item, metric, judge]`
//! rows: the same time-series id pattern as the service's traces, so a run's results
//! are one record-range scan.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use margin_service::db::{KeyPart, Store};
use serde::{Deserialize, Serialize};

pub const EVAL_RUN: &str = "eval_run";
pub const EVAL_RESULT: &str = "eval_result";

/// The runner's data folder: `MARGIN_EVAL_DATA`, or `evals/` beside the service's
/// data under Application Support.
pub fn default_data_dir() -> PathBuf {
    match std::env::var_os("MARGIN_EVAL_DATA") {
        Some(p) if !p.is_empty() => PathBuf::from(p),
        _ => margin_service::config::app_support_dir().join("evals"),
    }
}

/// One run of a suite.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalRun {
    pub id: String,
    /// "retrieval", "courses", "agent", "questions", "judges".
    pub suite: String,
    pub variant: String,
    pub pipeline_hash: String,
    pub embedder_version: String,
    pub corpus_hash: String,
    /// `sha256(variant toml + corpus hash + embedder version)`.
    pub key: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    /// Judges and graders used, by id.
    #[serde(default)]
    pub judges: Vec<String>,
    /// False when a judge's canaries failed (the report says which).
    #[serde(default = "yes")]
    pub valid: bool,
    #[serde(default)]
    pub notes: Vec<String>,
    /// The suite's summary (its report struct as JSON).
    #[serde(default)]
    pub summary: serde_json::Value,
}

fn yes() -> bool {
    true
}

/// One metric of one item (query, lesson, task) of a run; `item` "_all" for a
/// run-level number.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EvalResult {
    pub run_id: String,
    pub at: DateTime<Utc>,
    pub item: String,
    /// The item's group (query kind, course id…).
    #[serde(default)]
    pub group: String,
    pub metric: String,
    pub value: f64,
    #[serde(default)]
    pub judge: Option<String>,
}

pub struct EvalStore {
    pub store: Store,
}

impl EvalStore {
    pub async fn open(dir: &Path) -> anyhow::Result<Self> {
        Self::init(Store::open(&dir.join("db")).await?).await
    }

    pub async fn memory() -> anyhow::Result<Self> {
        Self::init(Store::memory().await?).await
    }

    async fn init(store: Store) -> anyhow::Result<Self> {
        store
            .query_rows::<serde_json::Value>(
                "DEFINE TABLE IF NOT EXISTS eval_run SCHEMALESS; DEFINE TABLE IF NOT EXISTS eval_result SCHEMALESS;",
                vec![],
            )
            .await?;
        Ok(Self { store })
    }

    pub async fn put_run(&self, run: &EvalRun) -> anyhow::Result<()> {
        self.store.put(EVAL_RUN, &run.id, run).await
    }

    pub async fn run(&self, id: &str) -> anyhow::Result<Option<EvalRun>> {
        self.store.get(EVAL_RUN, id).await
    }

    /// Every run, newest first.
    pub async fn runs(&self) -> anyhow::Result<Vec<EvalRun>> {
        let mut runs: Vec<EvalRun> = self.store.list(EVAL_RUN).await?;
        runs.sort_by_key(|r| std::cmp::Reverse(r.started_at));
        Ok(runs)
    }

    /// The newest run of `suite` (any suite when `None`).
    pub async fn latest(&self, suite: Option<&str>) -> anyhow::Result<Option<EvalRun>> {
        Ok(self
            .runs()
            .await?
            .into_iter()
            .find(|r| suite.is_none_or(|s| r.suite == s)))
    }

    /// Writes results, 64 per statement.
    pub async fn add_results(&self, rows: &[EvalResult]) -> anyhow::Result<()> {
        for batch in rows.chunks(margin_service::db::INSERT_MANY_MAX) {
            self.store
                .query_rows::<serde_json::Value>(
                    "FOR $r IN $rows { CREATE type::record('eval_result', [$r.run_id, <datetime> $r.at, $r.item, $r.metric, $r.judge ?? '']) CONTENT $r RETURN NONE; };",
                    vec![("rows", serde_json::to_value(batch)?)],
                )
                .await?;
        }
        Ok(())
    }

    /// A run's results, in id order (time, then item, then metric).
    pub async fn results(&self, run_id: &str) -> anyhow::Result<Vec<EvalResult>> {
        let lo = DateTime::<Utc>::UNIX_EPOCH;
        let hi = DateTime::parse_from_rfc3339("9999-12-31T23:59:59Z")?.with_timezone(&Utc);
        self.store
            .range(
                EVAL_RESULT,
                &[KeyPart::Str(run_id.into()), KeyPart::Time(lo)],
                &[
                    KeyPart::Str(run_id.into()),
                    KeyPart::Time(hi),
                    KeyPart::Str("\u{10FFFF}".into()),
                ],
            )
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(id: &str, suite: &str, at: i64) -> EvalRun {
        EvalRun {
            id: id.into(),
            suite: suite.into(),
            variant: "baseline".into(),
            pipeline_hash: "h".into(),
            embedder_version: "v".into(),
            corpus_hash: "c".into(),
            key: "k".into(),
            started_at: DateTime::from_timestamp(at, 0).unwrap(),
            finished_at: None,
            judges: vec![],
            valid: true,
            notes: vec![],
            summary: serde_json::json!({"x": 1}),
        }
    }

    fn res(run: &str, item: &str, metric: &str, v: f64) -> EvalResult {
        EvalResult {
            run_id: run.into(),
            at: DateTime::from_timestamp(1_000, 0).unwrap(),
            item: item.into(),
            group: "semantic".into(),
            metric: metric.into(),
            value: v,
            judge: None,
        }
    }

    #[tokio::test]
    async fn runs_and_results_round_trip_by_range() {
        let s = EvalStore::memory().await.unwrap();
        s.put_run(&run("r1", "retrieval", 10)).await.unwrap();
        s.put_run(&run("r2", "courses", 20)).await.unwrap();
        assert_eq!(s.runs().await.unwrap()[0].id, "r2");
        assert_eq!(s.latest(Some("retrieval")).await.unwrap().unwrap().id, "r1");
        assert_eq!(s.run("r1").await.unwrap().unwrap().summary["x"], 1);
        let mut rows: Vec<EvalResult> = (0..100)
            .map(|i| res("r1", &format!("q{i:03}"), "recall_at_5", i as f64 / 100.0))
            .collect();
        rows.push(res("r10", "q000", "recall_at_5", 1.0));
        rows.push(res("r1", "q000", "ndcg_at_10", 0.5));
        s.add_results(&rows).await.unwrap();
        let got = s.results("r1").await.unwrap();
        assert_eq!(got.len(), 101, "r10's row isn't r1's");
        assert_eq!(got[0].item, "q000");
        assert!(
            got.iter()
                .any(|r| r.metric == "ndcg_at_10" && r.value == 0.5)
        );
        assert_eq!(s.results("r10").await.unwrap().len(), 1);
        assert!(s.results("none").await.unwrap().is_empty());
    }
}
