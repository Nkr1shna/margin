//! `margin-service scrub-restored`: the last step of a prod → dev restore
//! (`deploy/scripts/restore-into-dev.sh`, `docs/plan-deploy.md` §5). Deletes the rows
//! dev must not inherit from prod: every `device` (prod's device tokens must not sign
//! in to dev) and every trace row (dev evals must not read prod usage). The rollup
//! views over `trace` empty with it. Runs only against a `dev` config, with the service
//! stopped (the database is opened directly).

use anyhow::bail;
use serde::Deserialize;

use crate::config::Config;
use crate::db::{self, Store};

/// The tables emptied, in order: devices first, then the raw traces (their hourly and
/// daily views follow) and the rollup sums kept past retention.
pub const SCRUBBED_TABLES: [&str; 4] = [
    db::DEVICE,
    db::TRACE,
    db::TRACE_HOURLY_KEPT,
    db::TRACE_DAILY_KEPT,
];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scrubbed {
    pub table: &'static str,
    pub rows: u64,
}

#[derive(Deserialize)]
struct Count {
    n: u64,
}

async fn count(store: &Store, table: &'static str) -> anyhow::Result<u64> {
    let rows: Vec<Count> = store
        .query_rows(
            &format!("SELECT count() AS n FROM {table} GROUP ALL"),
            vec![],
        )
        .await?;
    Ok(rows.first().map(|c| c.n).unwrap_or(0))
}

/// Refuses anything but a dev config, so a slip never empties prod's devices.
pub fn check_environment(config: &Config) -> anyhow::Result<()> {
    if config.environment != "dev" {
        bail!(
            "scrub-restored only runs on a dev service; this config's environment is \"{}\"",
            config.environment
        );
    }
    Ok(())
}

/// Deletes every row of `SCRUBBED_TABLES` and says how many each had.
pub async fn scrub(store: &Store) -> anyhow::Result<Vec<Scrubbed>> {
    let mut out = Vec::new();
    for table in SCRUBBED_TABLES {
        let rows = count(store, table).await?;
        let _: Vec<serde_json::Value> = store
            .query_rows(&format!("DELETE {table} RETURN NONE"), vec![])
            .await?;
        out.push(Scrubbed { table, rows });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deletes_devices_and_traces_and_keeps_the_library() {
        let store = Store::memory().await.unwrap();
        store
            .put(db::DEVICE, "d1", &serde_json::json!({"name": "prod phone"}))
            .await
            .unwrap();
        store
            .put(db::TOPIC, "t1", &serde_json::json!({"title": "kept"}))
            .await
            .unwrap();
        let _: Vec<serde_json::Value> = store
            .query_rows(
                "CREATE trace CONTENT { kind: 'embed', at: time::now() } RETURN NONE; \
                 CREATE trace CONTENT { kind: 'embed', at: time::now() } RETURN NONE",
                vec![],
            )
            .await
            .unwrap();
        let done = scrub(&store).await.unwrap();
        assert_eq!(
            done,
            vec![
                Scrubbed {
                    table: db::DEVICE,
                    rows: 1
                },
                Scrubbed {
                    table: db::TRACE,
                    rows: 2
                },
                Scrubbed {
                    table: db::TRACE_HOURLY_KEPT,
                    rows: 0
                },
                Scrubbed {
                    table: db::TRACE_DAILY_KEPT,
                    rows: 0
                },
            ]
        );
        for table in SCRUBBED_TABLES {
            assert_eq!(count(&store, table).await.unwrap(), 0, "{table}");
        }
        assert_eq!(count(&store, db::TOPIC).await.unwrap(), 1);
    }

    #[test]
    fn refuses_prod() {
        let mut c = Config::default();
        assert!(check_environment(&c).is_err());
        c.environment = "dev".into();
        check_environment(&c).unwrap();
    }
}

// ---------------------------------------------------------------------------
// Phase 5: checked backups and the schema guard (`docs/phase5-brief.md` D5)
// ---------------------------------------------------------------------------

/// What `check_data` found in a data folder (a backup or a live copy).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataCheck {
    /// `SettingsRow.migrations`.
    pub migrations: Vec<String>,
    /// `SettingsRow.schema_version()`.
    pub schema_version: u32,
    /// Rows per table.
    pub rows: Vec<(String, u64)>,
    /// What is wrong: a Ready source or document without its file, a passage of a
    /// missing document, a missing table. Empty when the folder is sound.
    pub problems: Vec<String>,
}

/// `margin-service check-data <dir>`: copies `<dir>/db` to a temporary folder (a backup
/// is never opened in place), opens the copy, and checks it against `<dir>/sources`.
pub async fn check_data(_dir: &std::path::Path) -> anyhow::Result<DataCheck> {
    unimplemented!("P5, 5-data")
}

/// `margin-service backup [--label NAME]`: the service's own copy of the data folder,
/// as `deploy/scripts/backup-margin.sh` makes it (copy, then rename; the same keep
/// rule), also run before a schema migration. Returns the new backup's folder.
pub async fn backup(_config: &Config, _label: Option<&str>) -> anyhow::Result<std::path::PathBuf> {
    unimplemented!("P5, 5-data")
}
