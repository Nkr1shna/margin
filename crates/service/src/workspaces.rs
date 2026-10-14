//! Attached folders (table `workspace`). The service stores only what a device says
//! about a folder it holds; reading goes through that device (`devices::DeviceHub`).

use axum::Extension;
use axum::Json;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use margin_shared::{Id, NewWorkspace, Workspace};

use crate::auth::CurrentDevice;
use crate::error::{ApiJson, AppResult};
use crate::state::AppState;

/// `GET /api/workspaces`: every device's folders, oldest first.
pub async fn list_workspaces(State(state): State<AppState>) -> AppResult<Json<Vec<Workspace>>> {
    let _ = state;
    unimplemented!("service: workspaces::list_workspaces (phase 4, service owner)")
}

/// `POST /api/workspaces`: registers a folder held by the calling device (name and
/// location trimmed, not empty). Emits `WorkspaceChanged`.
pub async fn create_workspace(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    ApiJson(req): ApiJson<NewWorkspace>,
) -> AppResult<Json<Workspace>> {
    let _ = (state, device, req);
    unimplemented!("service: workspaces::create_workspace (phase 4, service owner)")
}

/// `DELETE /api/workspaces/{id}`: only from the device that holds it (403 "not_yours"
/// otherwise). Detaches it from every chat (`ChatChanged`), emits `WorkspaceDeleted`.
pub async fn delete_workspace(
    State(state): State<AppState>,
    Extension(CurrentDevice(device)): Extension<CurrentDevice>,
    Path(id): Path<Id>,
) -> AppResult<StatusCode> {
    let _ = (state, device, id);
    unimplemented!("service: workspaces::delete_workspace (phase 4, service owner)")
}

/// The workspaces with these ids that still exist, in the given order.
pub async fn by_ids(state: &AppState, ids: &[Id]) -> anyhow::Result<Vec<Workspace>> {
    let _ = (state, ids);
    unimplemented!("service: workspaces::by_ids (phase 1, service owner)")
}
