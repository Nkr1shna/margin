//! The owner-only export of one person's library (roadmap L2, `docs/l2-brief.md` D7,
//! owner Q3): `margin-service export --person <id> --out <dir>` on the service host.
//! It refuses while the service runs (it takes the database's lock, as `backup` does),
//! copies the store to a temporary folder (never opening the live data), writes
//! `<dir>/<person>/{topics,lessons,notes,chats,chat_items,sources,documents}.jsonl` and
//! the source files (`files/sources/<id>/`, `files/documents/<id>/`), and appends
//! `{at, person, by, host, out, ok}` to `<data>/exports.log`; the next start turns that
//! line into an `AuditKind::Export` row (`audit::start`). There is no API route: only
//! someone with a shell on the host, who can read the data folder, can run it.

use std::collections::HashSet;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::Utc;
use margin_shared::context::{Document, LibraryKind, Origin};
use serde_json::Value;

use crate::audit::ExportLine;
use crate::db::{self, Store};
use crate::people::PersonRow;
use crate::restore;

/// The file every export appends a line to, in the data folder.
pub const EXPORTS_LOG: &str = "exports.log";

/// What one export wrote.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Exported {
    /// The person id exported.
    pub person: String,
    /// `<out>/<person>`.
    pub dir: PathBuf,
    /// Rows written per file, in the order written.
    pub rows: Vec<(&'static str, usize)>,
    /// Folders of source and document files copied.
    pub files: usize,
}

/// Runs the export of `person`'s rows (an id, a handle or a GitHub login) from
/// `data_dir` (the service stopped) into `out`, printing what it wrote.
pub fn run(data_dir: &Path, person: &str, out: &Path) -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(10 * 1024 * 1024)
        .build()?;
    let done = runtime.block_on(export(data_dir, person, out, &unix_user(), &host_name()))?;
    for (file, n) in &done.rows {
        println!("wrote {n} rows to {file}");
    }
    println!("copied {} source and document folders", done.files);
    println!(
        "Exported the library of {} to {}. The export is logged in {} and shows in the \
         Audit view after the service's next start.",
        done.person,
        done.dir.display(),
        data_dir.join(EXPORTS_LOG).display()
    );
    Ok(())
}

/// The export itself, `by` (a unix user) on `host`. Refuses an open database, an unknown
/// person and a folder `<out>/<person>` that already holds files; once it starts
/// writing, it appends its line to `exports.log` whether it finishes or not (`ok`).
pub async fn export(
    data_dir: &Path,
    person: &str,
    out: &Path,
    by: &str,
    host: &str,
) -> anyhow::Result<Exported> {
    let copy = restore::open_stopped_copy(data_dir).await?;
    let result = export_from(&copy.store, data_dir, person, out, by, host).await;
    copy.close().await;
    result
}

async fn export_from(
    store: &Store,
    data_dir: &Path,
    person: &str,
    out: &Path,
    by: &str,
    host: &str,
) -> anyhow::Result<Exported> {
    let person = find_person(store, person).await?;
    let dir = out.join(&person);
    if dir.exists() && std::fs::read_dir(&dir)?.next().is_some() {
        anyhow::bail!(
            "{} already holds files; give another --out or empty it",
            dir.display()
        );
    }
    let at = Utc::now();
    let written = write_library(store, data_dir, &person, &dir).await;
    let line = ExportLine {
        at,
        person: person.clone(),
        by: by.to_string(),
        host: host.to_string(),
        out: absolute(&dir).display().to_string(),
        ok: written.is_ok(),
    };
    append_log(data_dir, &line).with_context(|| {
        format!(
            "the export ran but couldn't be logged in {}; tell the admins",
            data_dir.join(EXPORTS_LOG).display()
        )
    })?;
    let mut done = written?;
    done.person = person;
    done.dir = dir;
    Ok(done)
}

fn absolute(p: &Path) -> PathBuf {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf())
}

/// The person's id from an id, a handle or a GitHub login (case-insensitive).
async fn find_person(store: &Store, asked: &str) -> anyhow::Result<String> {
    if let Some(row) = store.get::<PersonRow>(db::PERSON, asked).await? {
        return Ok(row.person.id);
    }
    let wanted = asked.trim_start_matches('@').to_lowercase();
    let rows: Vec<PersonRow> = store.list(db::PERSON).await?;
    rows.into_iter()
        .find(|r| {
            r.person.handle.to_lowercase() == wanted
                || r.person.github_login.to_lowercase() == wanted
        })
        .map(|r| r.person.id)
        .with_context(|| {
            format!("there is no person {asked} in this data (give an id, handle or GitHub login)")
        })
}

fn id_of(row: &Value) -> Option<String> {
    row.get("id").and_then(Value::as_str).map(str::to_string)
}

fn owned_by(row: &Value, person: &str) -> bool {
    row.get("owner").and_then(Value::as_str) == Some(person)
}

/// The rows of `table` whose `owner` is `person`.
async fn owned(store: &Store, table: &'static str, person: &str) -> anyhow::Result<Vec<Value>> {
    let rows: Vec<Value> = store.list(table).await?;
    Ok(rows.into_iter().filter(|r| owned_by(r, person)).collect())
}

fn ids(rows: &[Value]) -> HashSet<String> {
    rows.iter().filter_map(id_of).collect()
}

async fn write_library(
    store: &Store,
    data_dir: &Path,
    person: &str,
    dir: &Path,
) -> anyhow::Result<Exported> {
    std::fs::create_dir_all(dir).with_context(|| format!("couldn't create {}", dir.display()))?;
    let mut done = Exported::default();
    let topics = owned(store, db::TOPIC, person).await?;
    let lessons = owned(store, db::LESSON, person).await?;
    let notes = owned(store, db::NOTE, person).await?;
    let chats = owned(store, db::CHAT, person).await?;
    let sources = owned(store, db::SOURCE, person).await?;
    let workspaces = owned(store, db::WORKSPACE, person).await?;
    let chat_ids = ids(&chats);
    let chat_items: Vec<Value> = store
        .list::<Value>(db::CHAT_ITEM)
        .await?
        .into_iter()
        .filter(|r| {
            r.get("chat_id")
                .and_then(Value::as_str)
                .is_some_and(|c| chat_ids.contains(c))
        })
        .collect();
    let (lesson_ids, note_ids, source_ids, workspace_ids) =
        (ids(&lessons), ids(&notes), ids(&sources), ids(&workspaces));
    // A document is the person's when it says so, or when the row it was made from is.
    let mut documents = Vec::new();
    for raw in store.list::<Value>(db::DOCUMENT).await? {
        let mine = owned_by(&raw, person)
            || match serde_json::from_value::<Document>(raw.clone()).map(|d| d.origin) {
                Ok(Origin::Source { source_id }) => source_ids.contains(&source_id),
                Ok(Origin::FolderFile { workspace_id, .. }) => {
                    workspace_ids.contains(&workspace_id)
                }
                Ok(Origin::Library { kind, id }) => match kind {
                    LibraryKind::Lesson => lesson_ids.contains(&id),
                    LibraryKind::Note => note_ids.contains(&id),
                    LibraryKind::Chat => chat_ids.contains(&id),
                },
                Ok(Origin::Agent { .. }) | Err(_) => false,
            };
        if mine {
            documents.push(raw);
        }
    }
    for (file, rows) in [
        ("topics.jsonl", &topics),
        ("lessons.jsonl", &lessons),
        ("notes.jsonl", &notes),
        ("chats.jsonl", &chats),
        ("chat_items.jsonl", &chat_items),
        ("sources.jsonl", &sources),
        ("documents.jsonl", &documents),
    ] {
        write_jsonl(&dir.join(file), rows)?;
        done.rows.push((file, rows.len()));
    }
    // The files: a source's bytes under `sources/<id>`, an agent's or folder file's
    // document under `documents/<id>`.
    for (folder, rows) in [
        ("sources", &sources),
        (crate::ingest::DOCUMENTS_DIR, &documents),
    ] {
        for id in rows.iter().filter_map(id_of) {
            let from = data_dir.join(folder).join(&id);
            if from.is_dir() {
                let to = dir.join("files").join(folder).join(&id);
                restore::copy_folder(&from, &to)
                    .with_context(|| format!("couldn't copy {}", from.display()))?;
                done.files += 1;
            }
        }
    }
    Ok(done)
}

fn write_jsonl(path: &Path, rows: &[Value]) -> anyhow::Result<()> {
    let mut f = std::io::BufWriter::new(
        std::fs::File::create(path)
            .with_context(|| format!("couldn't create {}", path.display()))?,
    );
    for row in rows {
        serde_json::to_writer(&mut f, row)?;
        f.write_all(b"\n")?;
    }
    f.into_inner().map_err(|e| e.into_error())?.sync_all()?;
    Ok(())
}

/// Appends one line to `<data>/exports.log` and syncs it.
pub fn append_log(data_dir: &Path, line: &ExportLine) -> anyhow::Result<()> {
    let mut text = serde_json::to_string(line)?;
    text.push('\n');
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(data_dir.join(EXPORTS_LOG))?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    Ok(())
}

/// Who runs the command: the user behind `sudo` when there is one.
fn unix_user() -> String {
    ["SUDO_USER", "USER", "LOGNAME"]
        .iter()
        .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()))
        .unwrap_or_else(|| "unknown".into())
}

fn host_name() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("HOSTNAME").ok())
        .unwrap_or_else(|| "unknown".into())
}

/// `--person <id> --out <dir>`, in either order.
pub fn parse_args(args: &[String]) -> anyhow::Result<(String, std::path::PathBuf)> {
    let mut person = None;
    let mut out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--person" => person = it.next().cloned(),
            "--out" => out = it.next().map(std::path::PathBuf::from),
            other => anyhow::bail!("unknown argument {other}"),
        }
    }
    match (person, out) {
        (Some(p), Some(o)) if !p.is_empty() => Ok((p, o)),
        _ => anyhow::bail!("give --person <id> and --out <dir>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arguments() {
        let a = |s: &[&str]| s.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        let (p, o) = parse_args(&a(&["--out", "/tmp/x", "--person", "p1"])).unwrap();
        assert_eq!((p.as_str(), o.as_path()), ("p1", Path::new("/tmp/x")));
        assert!(parse_args(&a(&["--person", "p1"])).is_err());
        assert!(parse_args(&a(&["--person", "p1", "--out", "o", "--x"])).is_err());
    }
}
