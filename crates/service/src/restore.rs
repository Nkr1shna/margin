//! `margin-service scrub-restored`: the last step of a prod → dev restore
//! (`deploy/scripts/restore-into-dev.sh`, `docs/plan-deploy.md` §5). Deletes the rows
//! dev must not inherit from prod: every `device` (prod's device tokens must not sign
//! in to dev) and every trace row (dev evals must not read prod usage). The rollup
//! views over `trace` empty with it. Runs only against a `dev` config, with the service
//! stopped (the database is opened directly).
//!
//! Also the data-integrity commands of phase 5 (`docs/phase5-brief.md` D5):
//! `margin-service check-data <dir>` (`check_data`), `margin-service backup`
//! (`backup`), and the schema guard every start goes through (`open_guarded`).

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
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

/// The folder beside `data_dir` that holds backups (`backup-margin.sh` writes there too).
pub const BACKUPS_DIR: &str = "backups";
/// A backup folder holding this file was checked (`check_data`) after it was written;
/// the file holds the check's report.
pub const CHECKED_MARKER: &str = "CHECKED";
/// How many backups `backup` keeps, like `backup-margin.sh`'s default.
pub const BACKUP_KEEP: usize = 10;
/// How long a closed database handle may take to let go of its files.
const RELEASE_WAIT: std::time::Duration = std::time::Duration::from_secs(30);
/// The lock file SurrealKV holds while a database is open.
const LOCK_FILE: &str = "LOCK";

/// The tables a data folder written at `db::SCHEMA_VERSION` 1 has (every open defines
/// them). A table added later comes with a schema bump and is required from that
/// version on, so an older backup isn't reported as broken for not having it.
pub fn required_tables(_schema_version: u32) -> Vec<&'static str> {
    vec![
        db::TOPIC,
        db::LESSON,
        db::NOTE,
        db::VIDEO,
        db::PROPOSAL,
        db::DEVICE,
        db::SETTINGS,
        db::SOURCE,
        db::CHAT,
        db::CHAT_ITEM,
        db::WORKSPACE,
        db::FOLDER_FILE,
        db::PROJECT,
        db::DOCUMENT,
        db::DOCUMENT_DELETED,
        db::PASSAGE,
        db::TRACE,
        db::TRACE_HOURLY_KEPT,
        db::TRACE_DAILY_KEPT,
    ]
}

/// What `check_data` found in a data folder (a backup or a copy of live data).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DataCheck {
    /// `SettingsRow.migrations`.
    pub migrations: Vec<String>,
    /// `SettingsRow.schema_version()`; 0 when the folder has no settings row yet.
    pub schema_version: u32,
    /// Rows per table, by table name.
    pub rows: Vec<(String, u64)>,
    /// Sources that aren't files of an attached folder: what `GET /api/library` lists
    /// (`restore-drill.sh` compares the two).
    pub library_sources: u64,
    /// What is wrong: a Ready source or document without its file, a passage of a
    /// missing document, a missing table. Empty when the folder is sound.
    pub problems: Vec<String>,
}

impl DataCheck {
    /// The rows of `table`, 0 when it has none.
    pub fn count(&self, table: &str) -> u64 {
        self.rows
            .iter()
            .find(|(t, _)| t == table)
            .map(|(_, n)| *n)
            .unwrap_or(0)
    }

    /// Machine-readable lines (`restore-drill.sh` reads `count <name> <n>`), then the
    /// problems, then the one-line verdict.
    pub fn report(&self, dir: &Path) -> String {
        let mut out = format!("schema_version {}\n", self.schema_version);
        let migrations = if self.migrations.is_empty() {
            "-".to_string()
        } else {
            self.migrations.join(",")
        };
        out.push_str(&format!("migrations {migrations}\n"));
        for (table, n) in &self.rows {
            out.push_str(&format!("count {table} {n}\n"));
        }
        out.push_str(&format!("count library_sources {}\n", self.library_sources));
        for p in &self.problems {
            out.push_str(&format!("problem {p}\n"));
        }
        out.push_str(&self.verdict(dir));
        out.push('\n');
        out
    }

    /// One line: sound, or how many problems.
    pub fn verdict(&self, dir: &Path) -> String {
        if self.problems.is_empty() {
            format!(
                "OK {} is sound (schema {}, {} tables, {} passages)",
                dir.display(),
                self.schema_version,
                self.rows.len(),
                self.count(db::PASSAGE)
            )
        } else {
            format!(
                "FAILED {} has {} problem(s)",
                dir.display(),
                self.problems.len()
            )
        }
    }
}

/// A temporary folder, removed when dropped.
struct TempDir(PathBuf);

impl TempDir {
    fn new(what: &str) -> std::io::Result<Self> {
        let dir = std::env::temp_dir().join(format!("margin-{what}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir)?;
        Ok(Self(dir))
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Copies the folder `src` to `dst` (created), keeping symbolic links as links.
/// `skip_locks` leaves out SurrealKV's `LOCK` files (a copy to open needs none).
fn copy_tree(src: &Path, dst: &Path, skip_locks: bool) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let to = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_tree(&entry.path(), &to, skip_locks)?;
        } else if ty.is_symlink() {
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(entry.path())?, &to)?;
            #[cfg(not(unix))]
            std::fs::copy(entry.path(), &to).map(|_| ())?;
        } else if !(skip_locks && entry.file_name() == LOCK_FILE) {
            std::fs::copy(entry.path(), &to)?;
        }
    }
    Ok(())
}

/// SurrealKV's lock files under a database folder.
fn lock_files(db_dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                if depth < 3 {
                    walk(&path, depth + 1, out);
                }
            } else if e.file_name() == LOCK_FILE {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(db_dir, 0, &mut out);
    out.sort();
    out
}

/// Takes the database's lock files as SurrealKV does when it opens it, so nothing opens
/// the database while they are held (dropping the files releases them). Fails when the
/// database is open, in this process or another one (the service is running).
fn lock_database(db_dir: &Path) -> anyhow::Result<Vec<std::fs::File>> {
    let mut held = Vec::new();
    for path in lock_files(db_dir) {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&path)
            .with_context(|| format!("couldn't open {}", path.display()))?;
        match file.try_lock() {
            Ok(()) => held.push(file),
            Err(std::fs::TryLockError::WouldBlock) => bail!(
                "the database in {} is open (is the service running?); stop it first: a \
                 copy of a live database isn't a backup",
                db_dir.display()
            ),
            Err(std::fs::TryLockError::Error(e)) => {
                return Err(e).with_context(|| format!("couldn't lock {}", path.display()));
            }
        }
    }
    Ok(held)
}

/// Waits until nothing holds the database open: a handle this process dropped lets go
/// of its files from background tasks, a moment later.
pub async fn wait_released(db_dir: &Path) -> anyhow::Result<()> {
    let start = std::time::Instant::now();
    loop {
        match lock_database(db_dir) {
            Ok(_) => return Ok(()),
            Err(e) if start.elapsed() >= RELEASE_WAIT => return Err(e),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(100)).await,
        }
    }
}

/// A copy of a stopped data folder's database, opened as it is (`open_stopped_copy`).
pub struct StoppedCopy {
    pub store: Store,
    db: PathBuf,
    _dir: TempDir,
}

impl StoppedCopy {
    /// Closes the copy and waits for its files to be let go before they are removed.
    pub async fn close(self) {
        let StoppedCopy { store, db, _dir } = self;
        drop(store);
        let _ = wait_released(&db).await;
    }
}

/// Copies `<data_dir>/db` to a temporary folder while holding the database's lock (so
/// it refuses while the service runs: a copy of a live database isn't consistent) and
/// opens the copy as it is: nothing defined, migrated or written, and the live data is
/// never opened. What `margin-service export` reads from.
pub async fn open_stopped_copy(data_dir: &Path) -> anyhow::Result<StoppedCopy> {
    let db_dir = data_dir.join("db");
    anyhow::ensure!(
        db_dir.is_dir(),
        "{} has no db folder: not a Margin data folder",
        data_dir.display()
    );
    let dir = TempDir::new("export")?;
    let copy_db = dir.0.join("db");
    {
        let (from, to) = (db_dir.clone(), copy_db.clone());
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _held = lock_database(&from)?;
            copy_tree(&from, &to, true).with_context(|| format!("couldn't copy {}", from.display()))
        })
        .await??;
    }
    let store = Store::open_existing(&copy_db)
        .await?
        .with_context(|| format!("{} holds no Margin database", db_dir.display()))?;
    Ok(StoppedCopy {
        store,
        db: copy_db,
        _dir: dir,
    })
}

/// Copies the folder `src` to `dst` (created), keeping links; for the export's files.
pub fn copy_folder(src: &Path, dst: &Path) -> std::io::Result<()> {
    copy_tree(src, dst, false)
}

/// Whether `dir` is missing or empty.
fn is_empty_dir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|mut d| d.next().is_none())
        .unwrap_or(true)
}

/// `margin-service check-data <dir>`: copies `<dir>/db` to a temporary folder (a backup
/// is never opened in place), opens the copy as it is (nothing defined or migrated),
/// and checks it against `<dir>/sources` and `<dir>/documents`: every table this schema
/// has, every Ready source's and file-backed document's file, every passage's document.
pub async fn check_data(dir: &Path) -> anyhow::Result<DataCheck> {
    let db_dir = dir.join("db");
    anyhow::ensure!(
        db_dir.is_dir(),
        "{} has no db folder: not a Margin data folder",
        dir.display()
    );
    let copy = TempDir::new("check")?;
    let copy_db = copy.0.join("db");
    {
        let (from, to) = (db_dir.clone(), copy_db.clone());
        tokio::task::spawn_blocking(move || copy_tree(&from, &to, true))
            .await?
            .with_context(|| format!("couldn't copy {}", db_dir.display()))?;
    }
    let check = match Store::open_existing(&copy_db).await? {
        Some(store) => {
            let check = check_store(&store, dir).await;
            drop(store);
            check
        }
        None => Err(anyhow::anyhow!(
            "{} holds no Margin database",
            db_dir.display()
        )),
    };
    // Let the copy's handle go before its folder is removed.
    let _ = wait_released(&copy_db).await;
    check
}

#[derive(Deserialize)]
struct DocPassages {
    document_id: String,
    n: u64,
}

/// The checks of `check_data` on an open store, against the files under `dir`.
async fn check_store(store: &Store, dir: &Path) -> anyhow::Result<DataCheck> {
    use margin_shared::context::{Document, DocumentStatus, Origin};
    use margin_shared::{Source, SourceStatus};

    let mut check = DataCheck::default();
    let tables = store.table_names().await?;
    let has = |t: &str| tables.iter().any(|x| x == t);
    let settings = if has(db::SETTINGS) {
        store
            .get::<serde_json::Value>(db::SETTINGS, crate::state::SETTINGS_ID)
            .await?
    } else {
        None
    };
    if let Some(s) = &settings {
        check.migrations = s
            .get("migrations")
            .and_then(|m| m.as_array())
            .map(|m| {
                m.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let raw = s
            .get("schema_version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        check.schema_version = u32::try_from(raw).unwrap_or(u32::MAX).max(1);
    }
    for table in required_tables(check.schema_version) {
        if !has(table) {
            check.problems.push(format!("the table {table} is missing"));
        }
    }
    for table in &tables {
        // The names come from the database's own catalog.
        let rows: Vec<Count> = store
            .query_rows(
                &format!("SELECT count() AS n FROM `{table}` GROUP ALL"),
                vec![],
            )
            .await?;
        let n = rows.first().map(|c| c.n).unwrap_or(0);
        check.rows.push((table.clone(), n));
    }

    let mut source_ids = std::collections::HashSet::new();
    let mut missing_sources = std::collections::HashSet::new();
    if has(db::SOURCE) {
        for row in store.list::<serde_json::Value>(db::SOURCE).await? {
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            match serde_json::from_value::<Source>(row) {
                Ok(source) => {
                    if source.workspace_id.is_none() {
                        check.library_sources += 1;
                    }
                    if matches!(source.status, SourceStatus::Ready { .. }) {
                        let file = dir
                            .join("sources")
                            .join(&source.id)
                            .join(crate::sources::FILE_NAME);
                        if !file.is_file() {
                            missing_sources.insert(source.id.clone());
                            check.problems.push(format!(
                                "the Ready source {} (\"{}\") has no file at {}",
                                source.id,
                                source.name,
                                file.display()
                            ));
                        }
                    }
                }
                Err(e) => check
                    .problems
                    .push(format!("the source {id} can't be read: {e}")),
            }
            source_ids.insert(id);
        }
    }
    let mut document_ids = std::collections::HashSet::new();
    if has(db::DOCUMENT) {
        for row in store.list::<serde_json::Value>(db::DOCUMENT).await? {
            let id = row
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            match serde_json::from_value::<Document>(row) {
                Ok(doc) => {
                    let has_file = !matches!(doc.origin, Origin::Library { .. });
                    if has_file && matches!(doc.status, DocumentStatus::Ready { .. }) {
                        // A source's (or uploaded folder file's) bytes are the source's;
                        // an agent's or unuploaded folder file's are the document's.
                        let folder = if source_ids.contains(&doc.id) {
                            "sources"
                        } else {
                            crate::ingest::DOCUMENTS_DIR
                        };
                        let file = dir
                            .join(folder)
                            .join(&doc.id)
                            .join(crate::sources::FILE_NAME);
                        // A source's missing file is reported once, with the source.
                        if !file.is_file() && !missing_sources.contains(&doc.id) {
                            check.problems.push(format!(
                                "the Ready document {} (\"{}\") has no file at {}",
                                doc.id,
                                doc.name,
                                file.display()
                            ));
                        }
                    }
                }
                Err(e) => check
                    .problems
                    .push(format!("the document {id} can't be read: {e}")),
            }
            document_ids.insert(id);
        }
    }
    if has(db::PASSAGE) {
        let per_doc: Vec<DocPassages> = store
            .query_rows(
                "SELECT document_id, count() AS n FROM passage GROUP BY document_id",
                vec![],
            )
            .await?;
        for p in per_doc {
            if !document_ids.contains(&p.document_id) {
                check.problems.push(format!(
                    "{} passage(s) belong to the missing document {}",
                    p.n, p.document_id
                ));
            }
        }
    }
    let old = unconverted_rows(store, &tables, &check.migrations).await?;
    if old > 0 {
        check.problems.push(unconverted_problem(old));
    }
    Ok(check)
}

/// A backup folder's name: `<date-time>[-label]`, as `backup-margin.sh` names them.
pub fn backup_name(at: chrono::DateTime<chrono::Local>, label: Option<&str>) -> String {
    let mut name = at.format("%Y-%m-%d-%H%M%S").to_string();
    if let Some(label) = label.map(clean_label).filter(|l| !l.is_empty()) {
        name.push('-');
        name.push_str(&label);
    }
    name
}

/// A label names a folder: anything but `A-Za-z0-9._-` becomes `-`, as in the script.
pub fn clean_label(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// Whether `name` is a backup folder's name (`YYYY-MM-DD-HHMMSS…`).
fn is_backup_name(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() >= 17
        && b.iter().take(17).enumerate().all(|(i, c)| match i {
            4 | 7 | 10 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

/// Removes all but the newest `keep` backups in `backups` (folders named like a backup
/// that hold `data/`, as the script's rule); 0 keeps all. Returns the folders removed.
pub fn prune_backups(backups: &Path, keep: usize) -> std::io::Result<Vec<PathBuf>> {
    if keep == 0 {
        return Ok(Vec::new());
    }
    let mut names: Vec<String> = std::fs::read_dir(backups)?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_backup_name(n))
        .collect();
    names.sort();
    names.reverse();
    let mut removed = Vec::new();
    for old in names.into_iter().skip(keep) {
        let path = backups.join(&old);
        if path.join("data").is_dir() {
            std::fs::remove_dir_all(&path)?;
            removed.push(path);
        }
    }
    Ok(removed)
}

/// How recent a checked `before-schema-<n>` backup must be for a start to reuse it
/// instead of making another: the restarts of a start that failed after its backup
/// (systemd retries every few seconds), not a later start on data restored since.
pub const MIGRATION_BACKUP_REUSE: chrono::TimeDelta = chrono::TimeDelta::hours(6);

/// The newest backup in `backups` named `<date-time>-<label>` that holds `data/` and
/// is `CHECKED`, made at most `MIGRATION_BACKUP_REUSE` before `now` (by its name's
/// stamp, local time); `None` when there is none.
pub fn checked_backup(
    backups: &Path,
    label: &str,
    now: chrono::DateTime<chrono::Local>,
) -> Option<PathBuf> {
    let suffix = format!("-{label}");
    let mut names: Vec<String> = std::fs::read_dir(backups)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| is_backup_name(n) && n.ends_with(&suffix) && n.len() == 17 + suffix.len())
        .collect();
    names.sort();
    names.into_iter().rev().find_map(|name| {
        let stamp = chrono::NaiveDateTime::parse_from_str(&name[..17], "%Y-%m-%d-%H%M%S")
            .ok()?
            .and_local_timezone(chrono::Local)
            .single()?;
        let age = now.signed_duration_since(stamp);
        let recent = age >= chrono::TimeDelta::zero() && age <= MIGRATION_BACKUP_REUSE;
        let dir = backups.join(&name);
        (recent && dir.join("data").is_dir() && dir.join(CHECKED_MARKER).is_file()).then_some(dir)
    })
}

/// The backups folder of a data folder: `<data_dir>/../backups`.
pub fn backups_dir(data_dir: &Path) -> PathBuf {
    data_dir
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(BACKUPS_DIR)
}

/// Copies `data_dir` to `<data_dir>/../backups/<date-time>[-label]/data` (to
/// `data.partial` first, then renamed), checks the copy (`check_data`) and marks the
/// folder `CHECKED`, or deletes it and fails; then keeps the newest `keep`. Holds the
/// database's lock while copying, so it refuses a database that is open.
pub async fn backup_data(
    data_dir: &Path,
    label: Option<&str>,
    keep: usize,
) -> anyhow::Result<PathBuf> {
    anyhow::ensure!(
        data_dir.is_dir(),
        "the data folder {} doesn't exist",
        data_dir.display()
    );
    let backups = backups_dir(data_dir);
    let dest = backups.join(backup_name(chrono::Local::now(), label));
    anyhow::ensure!(!dest.exists(), "{} already exists", dest.display());
    std::fs::create_dir_all(&dest)
        .with_context(|| format!("couldn't create {}", dest.display()))?;
    let copied = {
        let (from, partial) = (data_dir.to_path_buf(), dest.join("data.partial"));
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let _held = lock_database(&from.join("db"))?;
            copy_tree(&from, &partial, false)
                .with_context(|| format!("couldn't copy {}", from.display()))
        })
        .await?
    };
    if let Err(e) = copied {
        let _ = std::fs::remove_dir_all(&dest);
        return Err(e.context("no backup was made"));
    }
    let data = dest.join("data");
    std::fs::rename(dest.join("data.partial"), &data)?;
    let check = match check_data(&data).await {
        Ok(c) => c,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(e.context("the backup couldn't be checked, so it was deleted"));
        }
    };
    if !check.problems.is_empty() {
        let _ = std::fs::remove_dir_all(&dest);
        bail!(
            "the backup failed its check and was deleted:\n{}",
            check.problems.join("\n")
        );
    }
    let marker = dest.join(format!("{CHECKED_MARKER}.partial"));
    std::fs::write(&marker, check.report(&data))?;
    std::fs::rename(&marker, dest.join(CHECKED_MARKER))?;
    // The backup is complete and checked: an old copy that won't go is a warning, not
    // a failed backup (a start waiting on this backup must not fail over it).
    match prune_backups(&backups, keep) {
        Ok(removed) => {
            for old in removed {
                tracing::info!("removed the old backup {} (keeping {keep})", old.display());
            }
        }
        Err(e) => tracing::warn!(
            "couldn't prune the old backups in {}: {e}",
            backups.display()
        ),
    }
    Ok(dest)
}

/// `margin-service backup [--label NAME]`: the service's own copy of the data folder,
/// as `deploy/scripts/backup-margin.sh --check` makes it (copy, then rename; checked;
/// the same keep rule), also run before a schema migration. Returns the new backup's
/// folder. Refuses while the database is open (stop the service first).
pub async fn backup(config: &Config, label: Option<&str>) -> anyhow::Result<PathBuf> {
    backup_data(&config.data_dir, label, BACKUP_KEEP).await
}

/// What a data folder's stored schema version means for this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchemaGuard {
    /// No settings row yet: a new store, nothing to back up.
    Fresh,
    /// Written at this build's schema.
    Current,
    /// Written by an older build (0: before the field existed): back up, then migrate.
    Older(u32),
    /// Written by a newer build: refuse.
    Newer(u32),
}

/// The decision for a stored version (`Store::stored_schema_version`).
pub fn schema_guard(stored: Option<u32>) -> SchemaGuard {
    match stored {
        None => SchemaGuard::Fresh,
        Some(v) if v > db::SCHEMA_VERSION => SchemaGuard::Newer(v),
        Some(v) if v < db::SCHEMA_VERSION => SchemaGuard::Older(v),
        Some(_) => SchemaGuard::Current,
    }
}

/// What the service says, and why it doesn't start, on newer data.
pub fn newer_data_message(stored: u32) -> String {
    format!(
        "This data was last used by a newer Margin (schema {stored}); install that build or \
         restore the backup from before it."
    )
}

/// The label of the backup taken before migrating to `db::SCHEMA_VERSION`.
pub fn migration_label() -> String {
    format!("before-schema-{}", db::SCHEMA_VERSION)
}

/// The mark a store gets once its sources moved from `chunk`/`media` to `passage`
/// (`SettingsRow.migrations`). The conversion left with the build after ad6a5e69; a
/// store without the mark and with old rows refuses to start.
pub const PASSAGES_MIGRATION: &str = "passages-v1";

/// The tables of sources' vectors before the `passage` table. Old builds defined them on
/// every store, so their rows count, not their presence.
const OLD_TABLES: [&str; 2] = ["chunk", "media"];

/// Rows in `OLD_TABLES` of a store that never recorded `PASSAGES_MIGRATION`: 0 for every
/// converted or new store.
async fn unconverted_rows(
    store: &Store,
    tables: &[String],
    migrations: &[String],
) -> anyhow::Result<u64> {
    if migrations.iter().any(|m| m == PASSAGES_MIGRATION) {
        return Ok(0);
    }
    #[derive(Deserialize)]
    struct Count {
        count: u64,
    }
    let mut n = 0;
    for t in OLD_TABLES.iter().filter(|t| tables.iter().any(|x| x == *t)) {
        let rows: Vec<Count> = store
            .query_rows(&format!("SELECT count() FROM {t} GROUP ALL"), vec![])
            .await?;
        n += rows.first().map_or(0, |c| c.count);
    }
    Ok(n)
}

fn unconverted_problem(n: u64) -> String {
    format!("{n} old chunk/media rows, never converted")
}

/// Refuses a store whose sources were never moved to `passage`: this build no longer
/// reads the old rows, so it would search as if they were gone.
async fn refuse_unconverted(store: &Store) -> anyhow::Result<()> {
    let migrations = store
        .get::<serde_json::Value>(db::SETTINGS, crate::state::SETTINGS_ID)
        .await?
        .and_then(|s| serde_json::from_value::<Vec<String>>(s.get("migrations")?.clone()).ok())
        .unwrap_or_default();
    let n = unconverted_rows(store, &store.table_names().await?, &migrations).await?;
    anyhow::ensure!(
        n == 0,
        "this data has {}, which this build can't read. Start the previous release \
         (dev ad6a5e69) once with the embedding model downloaded, wait for its log line \
         \"every source is in the passage index\", then start this build",
        unconverted_problem(n)
    );
    Ok(())
}

/// Opens the configured database behind the schema guard: reads the stored schema
/// version without defining or migrating anything; refuses data a newer build wrote;
/// backs up data an older build wrote (`backup`, label `before-schema-<n>`) before this
/// build's `DEFINE`s and migrations touch it (a failed backup refuses to start); then
/// opens it as usual and stamps `db::SCHEMA_VERSION`. Returns the store and the backup
/// made, if any. Every by-hand start is as safe as a deploy.
pub async fn open_guarded(config: &Config) -> anyhow::Result<(Store, Option<PathBuf>)> {
    let db_dir = config.db_dir();
    let stored = if is_empty_dir(&db_dir) {
        None
    } else {
        // Fails early, with the clear message, when the service already runs.
        drop(lock_database(&db_dir)?);
        let stored = match Store::open_existing(&db_dir).await? {
            Some(store) => store.stored_schema_version().await?,
            None => None,
        };
        wait_released(&db_dir).await?;
        stored
    };
    let backup = match schema_guard(stored) {
        SchemaGuard::Fresh | SchemaGuard::Current => None,
        SchemaGuard::Newer(v) => bail!("{}", newer_data_message(v)),
        SchemaGuard::Older(v) => {
            let label = migration_label();
            // A start that failed after this backup (a bad DEFINE, a store that won't
            // open) is retried by systemd: one checked copy is enough, or each attempt
            // would add a copy and the keep rule would prune the good backups away.
            if let Some(dir) =
                checked_backup(&backups_dir(&config.data_dir), &label, chrono::Local::now())
            {
                tracing::info!(
                    "this data is at schema {v} and this build at {}: already backed up to {}",
                    db::SCHEMA_VERSION,
                    dir.display()
                );
                Some(dir)
            } else {
                tracing::info!(
                    "this data is at schema {v} and this build at {}: backing it up before \
                     migrating",
                    db::SCHEMA_VERSION
                );
                let dir = backup(config, Some(&label)).await.context(
                    "the backup before the schema migration failed, so nothing was migrated",
                )?;
                tracing::info!("backed up to {}", dir.display());
                Some(dir)
            }
        }
    };
    let store = Store::open(&db_dir).await?;
    refuse_unconverted(&store).await?;
    store.record_schema_version().await?;
    Ok((store, backup))
}

#[cfg(test)]
mod data_tests {
    use super::*;
    use crate::db::PassageRow;
    use margin_shared::context::{Document, DocumentStatus, LibraryKind, Origin};
    use margin_shared::{Source, SourceStatus};

    fn root() -> TempDir {
        TempDir::new("restore-test").unwrap()
    }

    fn config(root: &Path) -> Config {
        Config {
            data_dir: root.join("data"),
            ..Config::default()
        }
    }

    fn source(id: &str, ready: bool, workspace: Option<&str>) -> Source {
        Source {
            access: Vec::new(),
            deny: Vec::new(),
            owner: None,
            id: id.into(),
            topic_id: None,
            name: format!("{id}.md"),
            folder: None,
            kind: margin_embed::SourceKind::Text,
            bytes: 3,
            status: if ready {
                SourceStatus::Ready {
                    chunks: 1,
                    pages: None,
                    warning: None,
                }
            } else {
                SourceStatus::Failed { error: "x".into() }
            },
            created_at: chrono::Utc::now(),
            media_url: None,
            workspace_id: workspace.map(str::to_string),
            modified: None,
        }
    }

    fn document(id: &str, origin: Origin) -> Document {
        Document {
            owner: None,
            id: id.into(),
            project_id: "p".into(),
            origin,
            name: format!("{id}.md"),
            kind: margin_embed::SourceKind::Text,
            status: DocumentStatus::Ready { passages: 1 },
            generation: 1,
            bytes: 3,
            actor: "owner".into(),
            access: vec!["company".into()],
            deny: Vec::new(),
            created_at: chrono::Utc::now(),
            updated_at: chrono::Utc::now(),
        }
    }

    fn passage(id: &str, document_id: &str) -> PassageRow {
        PassageRow {
            id: id.into(),
            project_id: "p".into(),
            document_id: document_id.into(),
            origin: "source".into(),
            scope_ids: vec![],
            access: vec!["company".into()],
            deny: Vec::new(),
            unit_id: "u".into(),
            modality: "text".into(),
            ord: 0,
            page: None,
            start_line: None,
            end_line: None,
            start_ms: None,
            end_ms: None,
            section_path: String::new(),
            text: "hello".into(),
            file: None,
            chunk_hash: format!("h-{id}"),
            embedder: "test".into(),
            vector: vec![0.0; 768],
        }
    }

    /// A data folder with a Ready source (file present), a failed folder file, an agent
    /// document (file present), a library document (no file), passages and a topic.
    async fn seed(data: &Path) {
        let store = Store::open(&data.join("db")).await.unwrap();
        let settings = crate::state::SettingsRow {
            migrations: vec![PASSAGES_MIGRATION.into()],
            ..Default::default()
        };
        store
            .put(db::SETTINGS, crate::state::SETTINGS_ID, &settings)
            .await
            .unwrap();
        store
            .put(db::SOURCE, "s1", &source("s1", true, None))
            .await
            .unwrap();
        store
            .put(db::SOURCE, "s2", &source("s2", false, Some("w1")))
            .await
            .unwrap();
        let docs = [
            document(
                "s1",
                Origin::Source {
                    source_id: "s1".into(),
                },
            ),
            document("a1", Origin::Agent { name: "bot".into() }),
            document(
                "l1",
                Origin::Library {
                    kind: LibraryKind::Note,
                    id: "n1".into(),
                },
            ),
        ];
        for d in &docs {
            store.put(db::DOCUMENT, &d.id, d).await.unwrap();
        }
        store
            .insert_many(db::PASSAGE, &[passage("p1", "s1"), passage("p2", "a1")])
            .await
            .unwrap();
        store
            .put(db::TOPIC, "t1", &serde_json::json!({"title": "t"}))
            .await
            .unwrap();
        drop(store);
        for (folder, id) in [("sources", "s1"), (crate::ingest::DOCUMENTS_DIR, "a1")] {
            let dir = data.join(folder).join(id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(crate::sources::FILE_NAME), "abc").unwrap();
        }
        wait_released(&data.join("db")).await.unwrap();
    }

    /// Runs `sql` on the database in `db_dir` and closes it again.
    async fn run(db_dir: &Path, sql: &str, binds: Vec<(&'static str, serde_json::Value)>) {
        {
            let store = Store::open(db_dir).await.unwrap();
            let _: Vec<serde_json::Value> = store.query_rows(sql, binds).await.unwrap();
        }
        wait_released(db_dir).await.unwrap();
    }

    fn entries(dir: &Path) -> Vec<PathBuf> {
        let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
            .map(|d| d.flatten().map(|e| e.path()).collect())
            .unwrap_or_default();
        out.sort();
        out
    }

    #[test]
    fn the_guard_decides_by_version() {
        assert_eq!(schema_guard(None), SchemaGuard::Fresh);
        assert_eq!(schema_guard(Some(0)), SchemaGuard::Older(0));
        assert_eq!(schema_guard(Some(db::SCHEMA_VERSION)), SchemaGuard::Current);
        assert_eq!(
            schema_guard(Some(db::SCHEMA_VERSION + 1)),
            SchemaGuard::Newer(db::SCHEMA_VERSION + 1)
        );
        assert_eq!(
            newer_data_message(2),
            "This data was last used by a newer Margin (schema 2); install that build or \
             restore the backup from before it."
        );
        assert_eq!(
            migration_label(),
            format!("before-schema-{}", db::SCHEMA_VERSION)
        );
    }

    #[test]
    fn names_labels_and_the_keep_rule() {
        use chrono::TimeZone;
        let at = chrono::Local
            .with_ymd_and_hms(2026, 10, 14, 3, 30, 5)
            .unwrap();
        assert_eq!(backup_name(at, None), "2026-10-14-033005");
        assert_eq!(backup_name(at, Some("")), "2026-10-14-033005");
        assert_eq!(
            backup_name(at, Some("before schema/2")),
            "2026-10-14-033005-before-schema-2"
        );
        assert!(is_backup_name("2026-10-14-033005-nightly"));
        assert!(!is_backup_name("drills"));
        assert!(!is_backup_name("2026-10-14"));

        let root = root();
        let backups = root.0.join(BACKUPS_DIR);
        for n in 1..=5 {
            std::fs::create_dir_all(backups.join(format!("2026-10-0{n}-000000/data"))).unwrap();
        }
        std::fs::create_dir_all(backups.join("drills")).unwrap();
        // A folder without data/ (an interrupted copy) isn't the rule's to remove.
        std::fs::create_dir_all(backups.join("2026-09-01-000000")).unwrap();
        let removed = prune_backups(&backups, 3).unwrap();
        assert_eq!(
            removed,
            vec![
                backups.join("2026-10-02-000000"),
                backups.join("2026-10-01-000000")
            ]
        );
        assert!(backups.join("drills").is_dir());
        assert!(backups.join("2026-09-01-000000").is_dir());
        assert!(prune_backups(&backups, 0).unwrap().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_sound_folder_passes_and_a_missing_file_is_named() {
        let root = root();
        let data = root.0.join("data");
        seed(&data).await;
        let db_before = entries(&data.join("db"));

        let check = check_data(&data).await.unwrap();
        assert_eq!(check.problems, Vec::<String>::new());
        assert_eq!(check.migrations, vec![PASSAGES_MIGRATION.to_string()]);
        assert_eq!(check.schema_version, db::SCHEMA_VERSION);
        assert_eq!(check.count(db::SOURCE), 2);
        assert_eq!(check.count(db::DOCUMENT), 3);
        assert_eq!(check.count(db::PASSAGE), 2);
        assert_eq!(check.count(db::TOPIC), 1);
        assert_eq!(check.library_sources, 1);
        let report = check.report(&data);
        assert!(report.contains("\ncount passage 2\n"), "{report}");
        assert!(report.contains("\ncount library_sources 1\n"), "{report}");
        assert!(
            report.lines().last().unwrap().starts_with("OK "),
            "{report}"
        );
        // The folder itself was checked on a copy: its files are as they were.
        assert_eq!(entries(&data.join("db")), db_before);

        std::fs::remove_file(data.join("sources/s1/file")).unwrap();
        std::fs::remove_file(data.join(crate::ingest::DOCUMENTS_DIR).join("a1/file")).unwrap();
        let check = check_data(&data).await.unwrap();
        assert_eq!(check.problems.len(), 2, "{:?}", check.problems);
        assert!(
            check.problems[0].contains("source s1"),
            "{:?}",
            check.problems
        );
        assert!(
            check.problems[1].contains("document a1"),
            "{:?}",
            check.problems
        );
        assert!(check.verdict(&data).starts_with("FAILED "));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn orphan_passages_and_missing_tables_are_problems() {
        let root = root();
        let data = root.0.join("data");
        seed(&data).await;
        let db_dir = data.join("db");
        {
            let store = Store::open(&db_dir).await.unwrap();
            store
                .insert_many(db::PASSAGE, &[passage("p3", "gone")])
                .await
                .unwrap();
            let _: Vec<serde_json::Value> = store
                .query_rows("REMOVE TABLE proposal", vec![])
                .await
                .unwrap();
        }
        wait_released(&db_dir).await.unwrap();
        let check = check_data(&data).await.unwrap();
        assert!(
            check
                .problems
                .iter()
                .any(|p| p == "the table proposal is missing"),
            "{:?}",
            check.problems
        );
        assert!(
            check
                .problems
                .iter()
                .any(|p| p == "1 passage(s) belong to the missing document gone"),
            "{:?}",
            check.problems
        );
    }

    /// A store with the old tables, one `chunk` row or none, marked converted or not.
    async fn old_store(marked: bool, rows: bool) -> Store {
        let store = Store::memory().await.unwrap();
        if marked {
            let settings = crate::state::SettingsRow {
                migrations: vec![PASSAGES_MIGRATION.into()],
                ..Default::default()
            };
            store
                .put(db::SETTINGS, crate::state::SETTINGS_ID, &settings)
                .await
                .unwrap();
        }
        let _: Vec<serde_json::Value> = store
            .query_rows(
                "DEFINE TABLE chunk SCHEMALESS; DEFINE TABLE media SCHEMALESS",
                vec![],
            )
            .await
            .unwrap();
        if rows {
            store
                .put(
                    "chunk",
                    "s-0",
                    &serde_json::json!({"source_id": "s", "text": "old"}),
                )
                .await
                .unwrap();
        }
        store
    }

    #[tokio::test]
    async fn a_store_with_unconverted_rows_refuses_to_start() {
        let err = refuse_unconverted(&old_store(false, true).await)
            .await
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("1 old chunk/media rows, never converted"),
            "{err}"
        );
        assert!(err.contains("ad6a5e69"), "{err}");
        // Converted, or never had any: it starts.
        refuse_unconverted(&old_store(true, true).await)
            .await
            .unwrap();
        refuse_unconverted(&old_store(false, false).await)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn check_data_reports_unconverted_rows() {
        let root = root();
        let data = root.0.join("data");
        seed(&data).await;
        let db_dir = data.join("db");
        {
            let store = Store::open(&db_dir).await.unwrap();
            store
                .put(
                    db::SETTINGS,
                    crate::state::SETTINGS_ID,
                    &crate::state::SettingsRow::default(),
                )
                .await
                .unwrap();
            let _: Vec<serde_json::Value> = store
                .query_rows("DEFINE TABLE media SCHEMALESS", vec![])
                .await
                .unwrap();
            store
                .put("media", "s-m0", &serde_json::json!({"source_id": "s"}))
                .await
                .unwrap();
        }
        wait_released(&db_dir).await.unwrap();
        let check = check_data(&data).await.unwrap();
        assert!(
            check
                .problems
                .iter()
                .any(|p| p == "1 old chunk/media rows, never converted"),
            "{:?}",
            check.problems
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn check_data_refuses_a_folder_without_a_database() {
        let root = root();
        assert!(check_data(&root.0).await.is_err());
        std::fs::create_dir_all(root.0.join("db")).unwrap();
        let err = check_data(&root.0).await.unwrap_err();
        assert!(err.to_string().contains("no Margin database"), "{err:#}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backup_copies_checks_marks_and_keeps() {
        let root = root();
        let data = root.0.join("data");
        seed(&data).await;
        let first = backup_data(&data, Some("by hand"), 2).await.unwrap();
        assert_eq!(first.parent().unwrap(), root.0.join(BACKUPS_DIR));
        let name = first.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.ends_with("-by-hand"), "{name}");
        assert!(first.join("data/db").is_dir());
        assert!(first.join("data/sources/s1/file").is_file());
        assert!(!first.join("data.partial").exists());
        let marker = std::fs::read_to_string(first.join(CHECKED_MARKER)).unwrap();
        assert!(marker.contains("count passage 2"), "{marker}");

        // Older backups beyond `keep` go; the two newest stay.
        for n in 1..=3 {
            std::fs::create_dir_all(root.0.join(format!("backups/2000-01-0{n}-000000/data")))
                .unwrap();
        }
        // Names are to the second: wait until one made now sorts after the first.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while backup_name(chrono::Local::now(), None) <= name {
            assert!(
                std::time::Instant::now() < deadline,
                "the clock stood still"
            );
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let second = backup_data(&data, None, 2).await.unwrap();
        assert_eq!(entries(&root.0.join(BACKUPS_DIR)), vec![first, second]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_backup_that_fails_its_check_is_deleted() {
        let root = root();
        let data = root.0.join("data");
        seed(&data).await;
        std::fs::remove_file(data.join("sources/s1/file")).unwrap();
        let err = backup_data(&data, None, 10).await.unwrap_err();
        assert!(format!("{err:#}").contains("source s1"), "{err:#}");
        assert_eq!(entries(&root.0.join(BACKUPS_DIR)), Vec::<PathBuf>::new());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn backup_refuses_an_open_database() {
        let root = root();
        let data = root.0.join("data");
        seed(&data).await;
        let store = Store::open(&data.join("db")).await.unwrap();
        let err = backup_data(&data, None, 10).await.unwrap_err();
        assert!(format!("{err:#}").contains("is open"), "{err:#}");
        assert_eq!(entries(&root.0.join(BACKUPS_DIR)), Vec::<PathBuf>::new());
        let err = open_guarded(&config(&root.0)).await.err().unwrap();
        assert!(format!("{err:#}").contains("is open"), "{err:#}");
        drop(store);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_guard_backs_up_older_data_before_migrating() {
        let root = root();
        let config = config(&root.0);
        seed(&config.data_dir).await;
        // A settings row from before the field: version 0.
        run(
            &config.db_dir(),
            "UPDATE settings:main UNSET doc.schema_version",
            vec![],
        )
        .await;

        let (store, backup) = open_guarded(&config).await.unwrap();
        let backup = backup.expect("a backup before the migration");
        let name = backup.file_name().unwrap().to_str().unwrap().to_string();
        assert!(name.ends_with(&migration_label()), "{name}");
        assert!(backup.join(CHECKED_MARKER).is_file());
        // The backup was checked (a row without the field reads as schema 1).
        assert!(
            std::fs::read_to_string(backup.join(CHECKED_MARKER))
                .unwrap()
                .starts_with("schema_version 1\n")
        );
        assert_eq!(
            store.stored_schema_version().await.unwrap(),
            Some(db::SCHEMA_VERSION)
        );
        drop(store);
        wait_released(&config.db_dir()).await.unwrap();

        // Current now: no second backup.
        let (store, again) = open_guarded(&config).await.unwrap();
        assert_eq!(again, None);
        drop(store);
        assert_eq!(entries(&root.0.join(BACKUPS_DIR)), vec![backup.clone()]);
        wait_released(&config.db_dir()).await.unwrap();

        // A start that failed after its backup is retried (systemd): still older, the
        // checked backup just made is reused, not copied again.
        run(
            &config.db_dir(),
            "UPDATE settings:main UNSET doc.schema_version",
            vec![],
        )
        .await;
        let (store, reused) = open_guarded(&config).await.unwrap();
        assert_eq!(reused, Some(backup.clone()));
        drop(store);
        assert_eq!(entries(&root.0.join(BACKUPS_DIR)), vec![backup]);
    }

    /// `checked_backup` wants the label, `data/`, `CHECKED` and a recent stamp.
    #[test]
    fn a_recent_checked_migration_backup_is_found() {
        use chrono::TimeZone;
        let root = root();
        let backups = root.0.join(BACKUPS_DIR);
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 14, 12, 0, 0)
            .unwrap();
        let make = |name: &str, data: bool, checked: bool| {
            let dir = backups.join(name);
            std::fs::create_dir_all(dir.join(if data { "data" } else { "x" })).unwrap();
            if checked {
                std::fs::write(dir.join(CHECKED_MARKER), "ok\n").unwrap();
            }
            dir
        };
        assert_eq!(checked_backup(&backups, "before-schema-2", now), None);
        make("2026-10-14-113000-before-schema-2", true, false);
        make("2026-10-14-114000-before-schema-2", false, true);
        make("2026-10-14-115000-before-schema-2-extra", true, true);
        make("2026-10-14-115500", true, true);
        assert_eq!(checked_backup(&backups, "before-schema-2", now), None);
        let old = make("2026-10-14-010000-before-schema-2", true, true);
        assert_eq!(checked_backup(&backups, "before-schema-2", now), None);
        let good = make("2026-10-14-100000-before-schema-2", true, true);
        assert_eq!(
            checked_backup(&backups, "before-schema-2", now),
            Some(good.clone())
        );
        // The newest recent one wins; a stamp in the future doesn't count.
        let newer = make("2026-10-14-115900-before-schema-2", true, true);
        make("2026-10-14-130000-before-schema-2", true, true);
        assert_eq!(
            checked_backup(&backups, "before-schema-2", now),
            Some(newer)
        );
        // Earlier in the day, every backup above is in the future except none at all.
        let early = chrono::Local
            .with_ymd_and_hms(2026, 10, 14, 0, 30, 0)
            .unwrap();
        assert_eq!(checked_backup(&backups, "before-schema-2", early), None);
        assert!(old.is_dir());
        assert_eq!(checked_backup(&backups, "before-schema-3", now), None);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_guard_refuses_newer_data_and_leaves_it_alone() {
        let root = root();
        let config = config(&root.0);
        seed(&config.data_dir).await;
        let newer = db::SCHEMA_VERSION + 1;
        run(
            &config.db_dir(),
            "UPDATE settings:main SET doc.schema_version = $v",
            vec![("v", serde_json::json!(newer))],
        )
        .await;
        let err = open_guarded(&config).await.err().unwrap();
        assert_eq!(err.to_string(), newer_data_message(newer));
        assert!(!root.0.join(BACKUPS_DIR).exists());
        wait_released(&config.db_dir()).await.unwrap();
        let store = Store::open_existing(&config.db_dir())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(store.stored_schema_version().await.unwrap(), Some(newer));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_new_store_opens_without_a_backup() {
        let root = root();
        let config = config(&root.0);
        let (store, backup) = open_guarded(&config).await.unwrap();
        assert_eq!(backup, None);
        assert_eq!(store.stored_schema_version().await.unwrap(), None);
        drop(store);
        assert!(!root.0.join(BACKUPS_DIR).exists());
    }

    #[tokio::test]
    async fn the_version_is_recorded_on_an_existing_row_only() {
        let store = Store::memory().await.unwrap();
        store.record_schema_version().await.unwrap();
        assert_eq!(store.stored_schema_version().await.unwrap(), None);
        store
            .put(
                db::SETTINGS,
                crate::state::SETTINGS_ID,
                &serde_json::json!({"migrations": []}),
            )
            .await
            .unwrap();
        assert_eq!(store.stored_schema_version().await.unwrap(), Some(0));
        store.record_schema_version().await.unwrap();
        assert_eq!(
            store.stored_schema_version().await.unwrap(),
            Some(db::SCHEMA_VERSION)
        );
    }
}
