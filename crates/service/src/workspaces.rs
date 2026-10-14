//! Attached folders (table `workspace`). The service stores what a device says about a
//! folder it holds; the folder's allow-listed files are synced to the service and read
//! there as sources (`folders.rs`), never read from the device.

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use chrono::Utc;
use margin_shared::{Chat, Event, Id, NewWorkspace, Workspace};

use crate::auth::CurrentDevice;
use crate::chats;
use crate::db;
use crate::error::{ApiJson, AppError, AppResult};
use crate::folders;
use crate::state::{AppState, new_id};

/// Longest folder name or location kept, in characters.
const MAX_FIELD_CHARS: usize = 500;

/// `GET /api/workspaces`: every device's folders, oldest first.
pub async fn list_workspaces(State(state): State<AppState>) -> AppResult<Json<Vec<Workspace>>> {
    let mut all: Vec<Workspace> = state.store.list(db::WORKSPACE).await?;
    all.sort_by(|a, b| a.created_at.cmp(&b.created_at).then(a.id.cmp(&b.id)));
    Ok(Json(all))
}

fn clean(value: &str, what: &str) -> AppResult<String> {
    let v = value.trim();
    if v.is_empty() {
        return Err(AppError::bad_request(format!(
            "The folder's {what} is missing."
        )));
    }
    Ok(v.chars().take(MAX_FIELD_CHARS).collect())
}

/// `POST /api/workspaces`: registers a folder held by the calling device (name and
/// location trimmed, not empty). Emits `WorkspaceChanged`.
pub async fn create_workspace(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    ApiJson(req): ApiJson<NewWorkspace>,
) -> AppResult<Json<Workspace>> {
    let workspace = Workspace {
        id: new_id(),
        name: clean(&req.name, "name")?,
        device_id: device.id.clone(),
        device_name: device.name.clone(),
        platform: req.platform,
        location: clean(&req.location, "location")?,
        created_at: Utc::now(),
        sync: Default::default(),
        unreadable: Vec::new(),
    };
    {
        let _w = state.write_lock.lock().await;
        state
            .store
            .put(db::WORKSPACE, &workspace.id, &workspace)
            .await?;
    }
    state.emit(Event::WorkspaceChanged {
        workspace: workspace.clone(),
    });
    Ok(Json(workspace))
}

/// `DELETE /api/workspaces/{id}`: only from the device that holds it (403 "not_yours"
/// otherwise). Detaches it from every chat (`ChatChanged`), emits `WorkspaceDeleted`.
pub async fn delete_workspace(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let _w = state.write_lock.lock().await;
    let workspace: Workspace = state
        .store
        .get(db::WORKSPACE, &id)
        .await?
        .ok_or_else(|| AppError::not_found("folder"))?;
    if workspace.device_id != device.id {
        return Err(AppError::new(
            StatusCode::FORBIDDEN,
            "not_yours",
            format!(
                "This folder is attached on {}; remove it there.",
                workspace.device_name
            ),
        ));
    }
    delete_locked(&state, &id).await?;
    Ok(StatusCode::NO_CONTENT)
}

/// Deletes a workspace: its files on the service (each source with its document,
/// passages and file: `SourceDeleted` each) and their rows, detaches it from every chat
/// (`ChatChanged` each), then emits `WorkspaceDeleted`. Lessons keep their citations.
/// Call with `write_lock` held.
pub async fn delete_locked(state: &AppState, id: &str) -> anyhow::Result<()> {
    for event in folders::delete_files_locked(state, id).await? {
        state.emit(event);
    }
    let chats: Vec<Chat> = state.store.list(db::CHAT).await?;
    for mut chat in chats {
        if chat.workspace_ids.iter().any(|w| w == id) {
            chat.workspace_ids.retain(|w| w != id);
            chats::save_chat_locked(state, &mut chat).await?;
        }
    }
    state.store.delete(db::WORKSPACE, id).await?;
    state.emit(Event::WorkspaceDeleted { id: id.to_string() });
    Ok(())
}

/// Deletes every workspace a revoked device held. Call with `write_lock` held.
pub async fn delete_for_device_locked(state: &AppState, device_id: &str) -> anyhow::Result<()> {
    let held: Vec<Workspace> = state
        .store
        .list_where(db::WORKSPACE, "device_id", device_id)
        .await?;
    for w in held {
        delete_locked(state, &w.id).await?;
    }
    Ok(())
}

/// The workspaces with these ids that still exist, in the given order.
pub async fn by_ids(state: &AppState, ids: &[Id]) -> anyhow::Result<Vec<Workspace>> {
    let mut out = Vec::with_capacity(ids.len());
    for id in ids {
        if let Some(w) = state.store.get::<Workspace>(db::WORKSPACE, id).await? {
            out.push(w);
        }
    }
    Ok(out)
}
