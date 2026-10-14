//! The files of attached folders, on the service (phase 4, `docs/agent-phase4-brief.md`
//! D7a').
//!
//! The device holding a folder sends a manifest of its allow-listed files (paths with
//! SHA-256 stamps) to `POST routes::workspace_sync(id)`; the service diffs it against
//! its `folder_file` rows (one per path: the stamp the device last listed and the stamp
//! of what the service received) and answers which paths to upload. Each upload becomes
//! (or replaces, keeping its id) a `Source` with `workspace_id`, read by the one ingest
//! pipeline like any source (`sources::create_or_replace`). Paths no longer listed lose
//! their source (document, passages, file) and row. `Workspace.sync` holds the counts
//! the app shows (`refresh_sync`).
//!
//! The rows are also the folder's live view for the agent's `list_files` and
//! `find_files` (`listing`, `find`): received files show their source's status, the rest
//! are waiting to be sent. Nothing here reads a file on disk or asks a device.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::time::{Duration, Instant};

use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::{Extension, Json};
use chrono::{DateTime, Utc};
use margin_shared::{
    Event, FileStamp, FolderFileType, Id, ManifestFile, Source, SourceStatus, SyncManifest,
    SyncPlan, Workspace, WorkspaceSync, folder_file_type, relative_components,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::auth::CurrentDevice;
use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::sources;
use crate::state::AppState;

/// `WorkspaceChanged` for a folder's counts goes out at most this often (one deferred
/// emit carries what changed meanwhile).
pub const SYNC_EMIT_INTERVAL: Duration = Duration::from_millis(500);
/// Largest manifest accepted, in bytes (about 200 bytes a file: some 300 000 files).
pub const MANIFEST_MAX_BYTES: usize = 64 * 1024 * 1024;

/// The body limit of the sync route (the default 2 MB would cap a folder at about
/// 10 000 files; the owner wants no cap).
pub fn manifest_limit() -> DefaultBodyLimit {
    DefaultBodyLimit::max(MANIFEST_MAX_BYTES)
}

/// What the service knows about one path of a folder: the stamp the device last
/// reported, and the source it became once received.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FolderFileRow {
    pub id: Id,
    pub workspace_id: Id,
    pub path: String,
    pub kind: FolderFileType,
    /// The stamp the last manifest listed for the path.
    pub listed: FileStamp,
    /// The source holding the file, once received; its stamp is `received`.
    #[serde(default)]
    pub source_id: Option<Id>,
    #[serde(default)]
    pub received: Option<FileStamp>,
    pub listed_at: DateTime<Utc>,
}

impl db::HasKey for FolderFileRow {
    fn key(&self) -> String {
        self.id.clone()
    }
}

impl FolderFileRow {
    /// The service holds exactly what the device last listed.
    pub fn is_current(&self, sources: &HashSet<&str>) -> bool {
        self.received.as_ref() == Some(&self.listed)
            && self
                .source_id
                .as_deref()
                .is_some_and(|id| sources.contains(id))
    }
}

/// The record key of a path's row: the same path of the same folder is always the same
/// row.
pub fn row_key(workspace_id: &str, path: &str) -> String {
    let mut h = Sha256::new();
    h.update(workspace_id.as_bytes());
    h.update([0u8]);
    h.update(path.as_bytes());
    hex::encode(&h.finalize()[..16])
}

/// A path from a device or the model as the service stores it: its components (see
/// `relative_components`) joined with `/`. `None` for a path that isn't one inside the
/// folder, or is the folder itself.
pub fn clean_path(path: &str) -> Option<String> {
    let parts = relative_components(path).ok()?;
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// The message of a refused path.
pub fn bad_path_text(path: &str) -> String {
    let why = match relative_components(path) {
        Err(e) => format!(": {e}"),
        Ok(_) => String::new(),
    };
    format!("“{path}” isn't a path inside the folder{why}.")
}

fn bad_path(path: &str) -> AppError {
    AppError::new(StatusCode::BAD_REQUEST, "bad_path", bad_path_text(path))
}

// ---------------------------------------------------------------------------
// The diff
// ---------------------------------------------------------------------------

/// What a manifest changes.
#[derive(Debug, Default)]
pub struct Diff {
    /// Paths to upload, in path order.
    pub upload: Vec<String>,
    /// Paths already current.
    pub kept: u32,
    /// Rows to write (new, or with a new listed stamp).
    pub write: Vec<FolderFileRow>,
    /// Rows of paths the manifest no longer lists.
    pub gone: Vec<FolderFileRow>,
    /// Sources of the folder to delete: those of gone rows, and any of the folder's
    /// sources neither listed nor held by a row.
    pub delete_sources: Vec<Id>,
}

impl Diff {
    /// How many files the service removes.
    pub fn removed(&self) -> u32 {
        let without_source = self.gone.iter().filter(|r| r.source_id.is_none()).count();
        (self.delete_sources.len() + without_source) as u32
    }
}

/// Diffs a manifest (paths already cleaned and filtered, one entry per path) against a
/// folder's rows and sources. Pure: the caller writes the result.
pub fn diff(
    workspace_id: &str,
    rows: Vec<FolderFileRow>,
    sources: &[Source],
    manifest: &[ManifestFile],
    now: DateTime<Utc>,
) -> Diff {
    let source_ids: HashSet<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    let by_name: HashMap<&str, &str> = sources
        .iter()
        .map(|s| (s.name.as_str(), s.id.as_str()))
        .collect();
    let mut rows: HashMap<String, FolderFileRow> =
        rows.into_iter().map(|r| (r.path.clone(), r)).collect();
    let mut out = Diff::default();
    let mut listed: BTreeMap<&str, &ManifestFile> = BTreeMap::new();
    for f in manifest {
        listed.entry(f.path.as_str()).or_insert(f);
    }
    let mut held: HashSet<String> = HashSet::new();
    for (path, f) in &listed {
        let (mut row, mut changed) = match rows.remove(*path) {
            Some(row) => (row, false),
            None => (
                FolderFileRow {
                    id: row_key(workspace_id, path),
                    workspace_id: workspace_id.to_string(),
                    path: path.to_string(),
                    kind: f.kind,
                    listed: f.stamp.clone(),
                    source_id: None,
                    received: None,
                    listed_at: now,
                },
                true,
            ),
        };
        // A source of the path the row doesn't know (rows lost, or sent before a
        // manifest): the next upload replaces it in place.
        if row
            .source_id
            .as_deref()
            .is_none_or(|s| !source_ids.contains(s))
            && let Some(s) = by_name.get(path)
        {
            row.source_id = Some(s.to_string());
            row.received = None;
            changed = true;
        }
        if let Some(s) = &row.source_id {
            held.insert(s.clone());
        }
        // `listed_at` is when this stamp was first listed.
        if row.listed != f.stamp || row.kind != f.kind {
            row.listed = f.stamp.clone();
            row.kind = f.kind;
            row.listed_at = now;
            changed = true;
        }
        if row.is_current(&source_ids) {
            out.kept += 1;
        } else {
            out.upload.push(path.to_string());
        }
        if changed {
            out.write.push(row);
        }
    }
    let mut gone: Vec<FolderFileRow> = rows.into_values().collect();
    gone.sort_by(|a, b| a.path.cmp(&b.path));
    for r in &gone {
        if let Some(s) = &r.source_id
            && source_ids.contains(s.as_str())
            && !held.contains(s)
        {
            held.insert(s.clone());
            out.delete_sources.push(s.clone());
        }
    }
    for s in sources {
        if !held.contains(&s.id) {
            out.delete_sources.push(s.id.clone());
        }
    }
    out.gone = gone;
    out
}

/// A folder's counts from its rows and sources.
pub fn counts(
    rows: &[FolderFileRow],
    sources: &[Source],
    last_synced_at: Option<DateTime<Utc>>,
) -> WorkspaceSync {
    let ids: HashSet<&str> = sources.iter().map(|s| s.id.as_str()).collect();
    WorkspaceSync {
        files: sources.len() as u32,
        bytes: sources.iter().map(|s| s.bytes).sum(),
        uploading: rows.iter().filter(|r| !r.is_current(&ids)).count() as u32,
        reading: sources
            .iter()
            .filter(|s| sources::is_pending(&s.status))
            .count() as u32,
        failed: sources
            .iter()
            .filter(|s| matches!(s.status, SourceStatus::Failed { .. }))
            .count() as u32,
        last_synced_at,
    }
}

pub async fn rows_of(state: &AppState, workspace_id: &str) -> anyhow::Result<Vec<FolderFileRow>> {
    state
        .store
        .list_where(db::FOLDER_FILE, "workspace_id", workspace_id)
        .await
}

pub async fn sources_of(state: &AppState, workspace_id: &str) -> anyhow::Result<Vec<Source>> {
    state
        .store
        .list_where(db::SOURCE, "workspace_id", workspace_id)
        .await
}

async fn workspace(state: &AppState, id: &str) -> AppResult<Workspace> {
    state
        .store
        .get(db::WORKSPACE, id)
        .await?
        .ok_or_else(|| AppError::not_found("folder"))
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// `POST /api/workspaces/{id}/sync`: the holding device's manifest → what to upload.
pub async fn sync(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    Path(id): Path<Id>,
    ApiJson(manifest): ApiJson<SyncManifest>,
) -> AppResult<Json<SyncPlan>> {
    // Paths checked and the types filtered before anything is touched.
    let mut files = Vec::with_capacity(manifest.files.len());
    for f in manifest.files {
        let Some(path) = clean_path(&f.path) else {
            return Err(bad_path(&f.path));
        };
        // The device filters first; this is the second line of defence.
        if folder_file_type(&path) != Some(f.kind) {
            continue;
        }
        files.push(ManifestFile { path, ..f });
    }
    let now = Utc::now();
    let (plan, events, ws) = {
        let _w = state.write_lock.lock().await;
        let mut ws = workspace(&state, &id).await?;
        if ws.device_id != device.id {
            return Err(AppError::new(
                StatusCode::FORBIDDEN,
                "not_yours",
                format!(
                    "Only the device holding this folder can sync it ({}).",
                    ws.device_name
                ),
            ));
        }
        let rows = rows_of(&state, &id).await?;
        let all = sources_of(&state, &id).await?;
        let d = diff(&id, rows, &all, &files, now);
        let mut events = Vec::new();
        for s in &d.delete_sources {
            events.push(sources::delete_locked(&state, s).await?);
        }
        for r in &d.gone {
            state.store.delete(db::FOLDER_FILE, &r.id).await?;
        }
        state.store.put_many(db::FOLDER_FILE, &d.write).await?;
        let rows = rows_of(&state, &id).await?;
        let deleted: HashSet<&str> = d.delete_sources.iter().map(String::as_str).collect();
        let left: Vec<Source> = all
            .into_iter()
            .filter(|s| !deleted.contains(s.id.as_str()))
            .collect();
        ws.sync = counts(&rows, &left, Some(now));
        state.store.put(db::WORKSPACE, &id, &ws).await?;
        let plan = SyncPlan {
            removed: d.removed(),
            upload: d.upload,
            kept: d.kept,
        };
        (plan, events, ws)
    };
    for e in events {
        state.emit(e);
    }
    emit_changed(&state, ws);
    Ok(Json(plan))
}

/// `GET /api/workspaces/{id}/files`: the folder's sources, newest first.
pub async fn files(
    State(state): State<AppState>,
    Path(id): Path<Id>,
) -> AppResult<Json<Vec<Source>>> {
    workspace(&state, &id).await?;
    let mut all = sources_of(&state, &id).await?;
    all.sort_by(|a, b| b.created_at.cmp(&a.created_at).then(a.name.cmp(&b.name)));
    Ok(Json(all.iter().map(|s| state.public_source(s)).collect()))
}

// ---------------------------------------------------------------------------
// Upkeep
// ---------------------------------------------------------------------------

/// Records that the service now holds `path` of a folder as `source_id`, with the stamp
/// of what it received. Call with the write lock held.
pub async fn record_received_locked(
    state: &AppState,
    workspace_id: &str,
    path: &str,
    kind: FolderFileType,
    source_id: &str,
    stamp: FileStamp,
) -> anyhow::Result<()> {
    let key = row_key(workspace_id, path);
    let mut row = match state
        .store
        .get::<FolderFileRow>(db::FOLDER_FILE, &key)
        .await?
    {
        Some(r) => r,
        // Sent before any manifest listed it: what arrived is what is listed.
        None => FolderFileRow {
            id: key.clone(),
            workspace_id: workspace_id.to_string(),
            path: path.to_string(),
            kind,
            listed: stamp.clone(),
            source_id: None,
            received: None,
            listed_at: Utc::now(),
        },
    };
    row.source_id = Some(source_id.to_string());
    row.received = Some(stamp);
    state.store.put(db::FOLDER_FILE, &key, &row).await
}

/// Recomputes a folder's `Workspace.sync` from its rows and sources (keeping
/// `last_synced_at`), saves it and sends `WorkspaceChanged` (coalesced). Call without
/// the write lock. A folder that is gone is skipped.
pub async fn refresh_sync(state: &AppState, workspace_id: &str) -> anyhow::Result<()> {
    let ws = {
        let _w = state.write_lock.lock().await;
        let Some(mut ws) = state
            .store
            .get::<Workspace>(db::WORKSPACE, workspace_id)
            .await?
        else {
            return Ok(());
        };
        let rows = rows_of(state, workspace_id).await?;
        let all = sources_of(state, workspace_id).await?;
        let sync = counts(&rows, &all, ws.sync.last_synced_at);
        if sync == ws.sync {
            return Ok(());
        }
        ws.sync = sync;
        state.store.put(db::WORKSPACE, workspace_id, &ws).await?;
        ws
    };
    emit_changed(state, ws);
    Ok(())
}

/// `refresh_sync`, logging a failure (for callers that go on regardless).
pub async fn refresh_sync_logged(state: &AppState, workspace_id: &str) {
    if let Err(e) = refresh_sync(state, workspace_id).await {
        tracing::warn!("couldn't update the counts of folder {workspace_id}: {e:#}");
    }
}

/// When a folder's `WorkspaceChanged` last went out, and whether one is waiting.
#[derive(Debug, Clone, Copy)]
pub struct EmitSlot {
    last: Instant,
    deferred: bool,
}

/// Sends `WorkspaceChanged` now, or, when one went out less than `SYNC_EMIT_INTERVAL`
/// ago, once that much time has passed (with the folder as it is then).
pub fn emit_changed(state: &AppState, ws: Workspace) {
    let now = Instant::now();
    let wait = {
        let mut slots = state
            .folder_sync_emits
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        match slots.get_mut(&ws.id) {
            Some(slot) if slot.deferred => return,
            Some(slot) if now.duration_since(slot.last) < SYNC_EMIT_INTERVAL => {
                slot.deferred = true;
                Some(SYNC_EMIT_INTERVAL - now.duration_since(slot.last))
            }
            _ => {
                slots.insert(
                    ws.id.clone(),
                    EmitSlot {
                        last: now,
                        deferred: false,
                    },
                );
                None
            }
        }
    };
    let Some(wait) = wait else {
        state.emit(Event::WorkspaceChanged { workspace: ws });
        return;
    };
    let state = state.clone();
    tokio::spawn(async move {
        tokio::time::sleep(wait).await;
        state
            .folder_sync_emits
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                ws.id.clone(),
                EmitSlot {
                    last: Instant::now(),
                    deferred: false,
                },
            );
        match state.store.get::<Workspace>(db::WORKSPACE, &ws.id).await {
            Ok(Some(now)) => state.emit(Event::WorkspaceChanged { workspace: now }),
            Ok(None) => {}
            Err(e) => tracing::warn!("couldn't read folder {}: {e:#}", ws.id),
        }
    });
}

/// Deletes a folder's sources (each with its document, passages and file) and rows.
/// Call with the write lock held; returns the events to send.
pub async fn delete_files_locked(
    state: &AppState,
    workspace_id: &str,
) -> anyhow::Result<Vec<Event>> {
    let mut events = Vec::new();
    for s in sources_of(state, workspace_id).await? {
        events.push(sources::delete_locked(state, &s.id).await?);
    }
    state
        .store
        .delete_where(db::FOLDER_FILE, "workspace_id", workspace_id)
        .await?;
    state
        .folder_sync_emits
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(workspace_id);
    Ok(events)
}

// ---------------------------------------------------------------------------
// What the agent's list_files and find_files see
// ---------------------------------------------------------------------------

/// One file of a folder as the service has it.
#[derive(Debug, Clone)]
pub struct FileView {
    pub row: FolderFileRow,
    /// The source, once received.
    pub source: Option<Source>,
}

/// An entry of `listing`.
#[derive(Debug, Clone)]
pub enum Entry {
    /// A subfolder (its path ends with `/`) and how many files are under it.
    Folder {
        path: String,
        files: u32,
    },
    File(Box<FileView>),
}

/// The folder's files (rows joined to their sources), in path order. Sources without a
/// row (sent before the first manifest; rare) are listed as received.
pub async fn files_of(state: &AppState, workspace_id: &str) -> anyhow::Result<Vec<FileView>> {
    let rows = rows_of(state, workspace_id).await?;
    let all = sources_of(state, workspace_id).await?;
    let mut by_id: HashMap<String, Source> = all.into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut out: Vec<FileView> = rows
        .into_iter()
        .map(|row| {
            let source = row.source_id.as_ref().and_then(|id| by_id.remove(id));
            FileView { row, source }
        })
        .collect();
    for (_, s) in by_id {
        let kind = folder_file_type(&s.name).unwrap_or(FolderFileType::Text);
        let stamp = FileStamp {
            bytes: s.bytes,
            modified: s.modified,
            sha256: String::new(),
        };
        out.push(FileView {
            row: FolderFileRow {
                id: row_key(workspace_id, &s.name),
                workspace_id: workspace_id.to_string(),
                path: s.name.clone(),
                kind,
                listed: stamp.clone(),
                source_id: Some(s.id.clone()),
                received: Some(stamp),
                listed_at: s.created_at,
            },
            source: Some(s),
        });
    }
    out.sort_by(|a, b| a.row.path.cmp(&b.row.path));
    Ok(out)
}

/// `path` relative to `under` ("" is the root), when it is inside it.
fn relative_to<'a>(path: &'a str, under: &str) -> Option<&'a str> {
    if under.is_empty() {
        return Some(path);
    }
    path.strip_prefix(under)?.strip_prefix('/')
}

/// What is directly inside `under` ("" for the root): its subfolders (with how many
/// files are under each) first, then its files, each in name order.
pub fn listing(files: Vec<FileView>, under: &str) -> Vec<Entry> {
    let mut folders: BTreeMap<String, u32> = BTreeMap::new();
    let mut out_files = Vec::new();
    for f in files {
        let Some(rel) = relative_to(&f.row.path, under) else {
            continue;
        };
        match rel.split_once('/') {
            Some((dir, _)) => *folders.entry(format!("{dir}/")).or_default() += 1,
            None => out_files.push(Entry::File(Box::new(f))),
        }
    }
    folders
        .into_iter()
        .map(|(path, files)| Entry::Folder { path, files })
        .chain(out_files)
        .collect()
}

/// The files anywhere under `under` whose path relative to it matches `pattern` (a
/// glob: `*` within a name, `**` across folders, `?` one character; case-insensitive).
/// `Err` is the message for a bad pattern.
pub fn find(files: Vec<FileView>, under: &str, pattern: &str) -> Result<Vec<FileView>, String> {
    let glob = globset::GlobBuilder::new(pattern)
        .case_insensitive(true)
        .literal_separator(true)
        .build()
        .map_err(|e| format!("“{pattern}” isn't a valid pattern: {e}"))?
        .compile_matcher();
    Ok(files
        .into_iter()
        .filter(|f| relative_to(&f.row.path, under).is_some_and(|rel| glob.is_match(rel)))
        .collect())
}

/// Whether `under` names a folder of the listing (some file is under it), or the root.
pub fn has_folder(files: &[FileView], under: &str) -> bool {
    under.is_empty()
        || files
            .iter()
            .any(|f| relative_to(&f.row.path, under).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(sha: &str) -> FileStamp {
        FileStamp {
            bytes: 10,
            modified: Some(DateTime::from_timestamp(1_760_000_000, 0).unwrap()),
            sha256: sha.into(),
        }
    }

    fn file(path: &str, sha: &str) -> ManifestFile {
        ManifestFile {
            path: path.into(),
            kind: folder_file_type(path).unwrap(),
            stamp: stamp(sha),
        }
    }

    fn source(id: &str, name: &str) -> Source {
        Source {
            id: id.into(),
            topic_id: None,
            name: name.into(),
            folder: Some("notes".into()),
            kind: margin_shared::SourceKind::Text,
            bytes: 10,
            status: SourceStatus::Ready {
                chunks: 1,
                pages: None,
                warning: None,
            },
            created_at: Utc::now(),
            media_url: None,
            workspace_id: Some("w".into()),
            modified: None,
        }
    }

    /// Applies a diff to rows and sources as the route does, then marks what was
    /// uploaded as received.
    fn apply(
        rows: Vec<FolderFileRow>,
        sources: Vec<Source>,
        d: &Diff,
        receive: bool,
    ) -> (Vec<FolderFileRow>, Vec<Source>) {
        let gone: HashSet<&str> = d.gone.iter().map(|r| r.id.as_str()).collect();
        let mut rows: BTreeMap<String, FolderFileRow> = rows
            .into_iter()
            .filter(|r| !gone.contains(r.id.as_str()))
            .map(|r| (r.id.clone(), r))
            .collect();
        for r in &d.write {
            rows.insert(r.id.clone(), r.clone());
        }
        let mut sources: Vec<Source> = sources
            .into_iter()
            .filter(|s| !d.delete_sources.contains(&s.id))
            .collect();
        if receive {
            for path in &d.upload {
                let row = rows.values_mut().find(|r| &r.path == path).unwrap();
                let id = row.source_id.clone().unwrap_or_else(|| format!("s-{path}"));
                if !sources.iter().any(|s| s.id == id) {
                    sources.push(source(&id, path));
                }
                row.source_id = Some(id);
                row.received = Some(row.listed.clone());
            }
        }
        (rows.into_values().collect(), sources)
    }

    #[test]
    fn diff_new_same_changed_gone_renamed_twice() {
        let t0 = Utc::now();
        // First sync: everything is new.
        let first = vec![
            file("same.md", "a"),
            file("changed.md", "b"),
            file("gone.md", "c"),
            file("old-name.md", "d"),
        ];
        let d = diff("w", vec![], &[], &first, t0);
        assert_eq!(
            d.upload,
            vec!["changed.md", "gone.md", "old-name.md", "same.md"]
        );
        assert_eq!((d.kept, d.removed()), (0, 0));
        let (rows, sources) = apply(vec![], vec![], &d, true);

        // Second: same, changed sha256 with the same bytes and time, gone, renamed, new.
        let second = vec![
            file("same.md", "a"),
            file("changed.md", "B"),
            file("new-name.md", "d"),
            file("new.md", "e"),
        ];
        for round in 0..2 {
            let d = diff("w", rows.clone(), &sources, &second, t0);
            assert_eq!(
                d.upload,
                vec!["changed.md", "new-name.md", "new.md"],
                "round {round}"
            );
            assert_eq!(d.kept, 1, "round {round}");
            assert_eq!(d.removed(), 2, "round {round}: gone.md and old-name.md");
            assert_eq!(
                d.delete_sources,
                vec!["s-gone.md".to_string(), "s-old-name.md".into()]
            );
            // A changed file keeps its source until the new copy replaces it.
            let changed = d.write.iter().find(|r| r.path == "changed.md").unwrap();
            assert_eq!(changed.source_id.as_deref(), Some("s-changed.md"));
        }
        let d = diff("w", rows.clone(), &sources, &second, t0);
        let (rows, sources) = apply(rows, sources, &d, false);
        // Sent again before anything arrived: the same uploads, nothing removed twice.
        let again = diff("w", rows.clone(), &sources, &second, t0);
        assert_eq!(again.upload, d.upload);
        assert_eq!((again.kept, again.removed()), (1, 0));
        // Once everything arrived: nothing to upload.
        let (rows, sources) = apply(rows, sources, &again, true);
        let done = diff("w", rows, &sources, &second, t0);
        assert!(done.upload.is_empty());
        assert_eq!((done.kept, done.removed()), (4, 0));
        assert!(done.write.is_empty(), "nothing changed, nothing written");
    }

    #[test]
    fn counts_follow_rows_and_sources() {
        let mut rows = Vec::new();
        let d = diff(
            "w",
            vec![],
            &[],
            &[file("a.md", "1"), file("b.md", "2")],
            Utc::now(),
        );
        rows.extend(d.write);
        let mut s = source("s1", "a.md");
        s.status = SourceStatus::Queued;
        rows[0].source_id = Some("s1".into());
        rows[0].received = Some(rows[0].listed.clone());
        let c = counts(&rows, &[s], None);
        assert_eq!(
            (c.files, c.bytes, c.uploading, c.reading, c.failed),
            (1, 10, 1, 1, 0)
        );
    }

    #[test]
    fn listing_and_find() {
        let d = diff(
            "w",
            vec![],
            &[],
            &[
                file("paper.pdf", "1"),
                file("ch1/a.md", "2"),
                file("ch1/deep/b.MD", "3"),
                file("figure.png", "4"),
            ],
            Utc::now(),
        );
        let files: Vec<FileView> = {
            let mut f: Vec<FileView> = d
                .write
                .into_iter()
                .map(|row| FileView { row, source: None })
                .collect();
            f.sort_by(|a, b| a.row.path.cmp(&b.row.path));
            f
        };
        let names = |entries: &[Entry]| -> Vec<String> {
            entries
                .iter()
                .map(|e| match e {
                    Entry::Folder { path, files } => format!("{path}({files})"),
                    Entry::File(f) => f.row.path.clone(),
                })
                .collect()
        };
        assert_eq!(
            names(&listing(files.clone(), "")),
            vec!["ch1/(2)", "figure.png", "paper.pdf"]
        );
        assert_eq!(
            names(&listing(files.clone(), "ch1")),
            vec!["deep/(1)", "ch1/a.md"]
        );
        let paths = |f: Vec<FileView>| f.into_iter().map(|f| f.row.path).collect::<Vec<_>>();
        assert_eq!(
            paths(find(files.clone(), "", "**/*.pdf").unwrap()),
            vec!["paper.pdf"]
        );
        assert_eq!(
            paths(find(files.clone(), "", "**/*.md").unwrap()),
            vec!["ch1/a.md", "ch1/deep/b.MD"]
        );
        assert_eq!(
            paths(find(files.clone(), "ch1", "*.md").unwrap()),
            vec!["ch1/a.md"]
        );
        assert!(
            find(files.clone(), "", "[")
                .unwrap_err()
                .contains("isn't a valid pattern")
        );
        assert!(has_folder(&files, "ch1/deep"));
        assert!(!has_folder(&files, "ch2"));
    }

    #[test]
    fn paths_are_cleaned() {
        assert_eq!(clean_path("./a//b.md").as_deref(), Some("a/b.md"));
        assert_eq!(clean_path("a\\b.md").as_deref(), Some("a/b.md"));
        assert_eq!(clean_path("../x"), None);
        assert_eq!(clean_path("/etc/x.md"), None);
        assert_eq!(clean_path(""), None);
        assert_eq!(clean_path(".env"), None);
        assert!(bad_path_text("../x").contains("isn't a path inside the folder"));
    }
}
