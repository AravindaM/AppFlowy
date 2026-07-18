use std::sync::{Arc, Weak};
use flowy_backup::SnapshotManifest;
use flowy_derive::Flowy_Event;
use flowy_error::{FlowyError, FlowyResult};
use flowy_user::user_manager::UserManager;
use flowy_folder::manager::FolderManager;
use flowy_database2::DatabaseManager;
use flowy_document::manager::DocumentManager;
use flowy_storage::manager::StorageManager;
use lib_dispatch::prelude::*;
use strum_macros::Display;

use crate::config::AppFlowyCoreConfig;
use crate::WorkspaceBackupResult;
use uuid::Uuid;
use tracing::{debug, error, warn};

/// All dependencies the backup handler needs, bundled into a single injected
/// state. lib-dispatch's `AFPluginHandler` only supports up to 5 handler
/// params, so the managers + config are grouped here rather than injected
/// individually.
pub struct BackupPluginState {
  pub user_manager: Weak<UserManager>,
  pub folder_manager: Weak<FolderManager>,
  pub database_manager: Weak<DatabaseManager>,
  pub document_manager: Weak<DocumentManager>,
  pub storage_manager: Weak<StorageManager>,
  pub config: Arc<AppFlowyCoreConfig>,
}

pub fn init(state: BackupPluginState) -> AFPlugin {
  AFPlugin::new()
    .name("Flowy-Backup")
    .state(state)
    .event(BackupEvent::BackupWorkspace, backup_workspace_handler)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Display, Hash, Flowy_Event)]
#[event_err = "FlowyError"]
pub enum BackupEvent {
  #[event()]
  BackupWorkspace = 0,
}

pub async fn backup_workspace_handler(
  state: AFPluginState<BackupPluginState>,
) -> Result<(), FlowyError> {
  let backup_result = execute_workspace_backup(
    state.user_manager.clone(),
    state.folder_manager.clone(),
    state.database_manager.clone(),
    state.document_manager.clone(),
    state.storage_manager.clone(),
    state.config.clone(),
  )
  .await?;

  if !backup_result.reopen_ok {
    warn!(
      "Backup completed but workspace reopen failed: {}",
      backup_result.reopen_error.as_deref().unwrap_or("unknown error")
    );
  }

  Ok(())
}

/// Execute workspace backup by taking a snapshot of the current collab_db.
/// Handles closing live collab objects, taking snapshot, and reopening/rebuilding managers.
pub async fn execute_workspace_backup(
  user_manager: Weak<UserManager>,
  folder_manager: Weak<FolderManager>,
  database_manager: Weak<DatabaseManager>,
  document_manager: Weak<DocumentManager>,
  storage_manager: Weak<StorageManager>,
  config: Arc<AppFlowyCoreConfig>,
) -> FlowyResult<WorkspaceBackupResult> {
  use flowy_user_pub::sql::select_user_workspace_type;

  let user_manager = user_manager
    .upgrade()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("UserManager no longer available"))?;

  let folder_manager = folder_manager
    .upgrade()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("FolderManager no longer available"))?;

  let database_manager = database_manager
    .upgrade()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("DatabaseManager no longer available"))?;

  let document_manager = document_manager
    .upgrade()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("DocumentManager no longer available"))?;

  let storage_manager = storage_manager
    .upgrade()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("StorageManager no longer available"))?;

  // Get current session to access user_id and workspace_id
  let session = user_manager.get_session()?;
  let user_id = session.user_id;
  let workspace_id = Uuid::parse_str(&session.workspace_id)?;

  // Query workspace type from database
  let mut conn = user_manager.db_connection(user_id)?;
  let workspace_type = select_user_workspace_type(&session.workspace_id, &mut conn)?;

  // Get user paths for snapshot location
  let data_root = config.storage_path.clone();
  // user data dir layout is `{data_root}/{uid}` (matches UserPaths::user_data_dir,
  // which is not public to this crate).
  let user_dir = std::path::PathBuf::from(&data_root).join(user_id.to_string());

  // SECURITY: the snapshot staging directory is derived here, backend-side, under
  // the app data root — never supplied by the caller/renderer. Accepting a caller
  // path would allow writing the full unencrypted workspace database to an
  // arbitrary filesystem location (path traversal / arbitrary write).
  let staging_dir = std::path::PathBuf::from(&data_root)
    .join("backups")
    .join("staging")
    .join(Uuid::new_v4().to_string());

  // FIX 1: Explicitly close all live collab objects for managers BEFORE closing collab_db
  // so their Weak references die and per-object flush plugins stop.
  debug!("Closing collab objects for backup quiesce");
  if let Err(err) = folder_manager.close_for_backup().await {
    error!("Failed to close folder manager for backup: {:?}", err);
  }
  if let Err(err) = database_manager.close_for_backup().await {
    error!("Failed to close database manager for backup: {:?}", err);
  }
  if let Err(err) = document_manager.close_for_backup().await {
    error!("Failed to close document manager for backup: {:?}", err);
  }

  // Capture snapshot result but always reopen/rebuild even on error (critical safety)
  let snapshot_result = async {
    // Step 2: Clear workspace awareness
    user_manager.clear_workspace_awareness(&workspace_id);
    debug!("Cleared workspace awareness");

    // Step 3: Close collab_db - this drops the sole Arc in UserDB::collab_db_map
    user_manager.close_collab_db(user_id)?;
    debug!("Closed collab_db");

    // Step 4: Take snapshot while collab_db is closed
    let snapshot_service = flowy_backup::SnapshotService::new(&data_root);
    let manifest = snapshot_service
      .snapshot(&user_dir, &staging_dir)
      .map_err(|e| FlowyError::internal().with_context(format!("backup snapshot failed: {e}")))?;
    debug!("Snapshot completed");

    Ok::<SnapshotManifest, FlowyError>(manifest)
  }
  .await;

  // Step 5: Reopen and rebuild - MUST always run even if snapshot failed
  debug!("Reopening and rebuilding managers after backup");
  let mut reopen_ok = true;
  let mut reopen_error: Option<String> = None;

  let reopen_result = async {
    // Use default data source (LocalDisk) for reopening
    // This matches the logic in on_workspace_opened when data exists on disk
    use flowy_folder::manager::FolderInitDataSource;
    let data_source = FolderInitDataSource::LocalDisk {
      create_if_not_exist: false,
    };

    // Reinitialize all managers in the same sequence as on_workspace_opened
    folder_manager
      .initialize_after_open_workspace(user_id, data_source)
      .await?;
    debug!("Reinitialized folder manager");

    database_manager
      .initialize_after_open_workspace(user_id, workspace_type.is_local())
      .await?;
    debug!("Reinitialized database manager");

    document_manager
      .initialize_after_open_workspace(user_id)
      .await?;
    debug!("Reinitialized document manager");

    // storage manager reinit returns unit; it logs its own failures internally
    storage_manager
      .initialize_after_open_workspace(&workspace_id)
      .await;
    debug!("Reinitialized storage manager");

    // Re-run user awareness initialization using the public wrapper
    user_manager
      .reinit_user_awareness(
        user_id,
        &session.user_uuid,
        &workspace_id,
        &workspace_type,
      )
      .await?;
    debug!("Reinitialized user awareness");

    Ok::<(), flowy_error::FlowyError>(())
  }
  .await;

  // FIX 3: Surface reopen failures explicitly
  if let Err(err) = reopen_result {
    reopen_ok = false;
    reopen_error = Some(err.to_string());
    warn!("Failed to reopen managers after backup: app may be degraded: {:?}", err);
  }

  // Return partial success result - snapshot success is separate from reopen success
  match snapshot_result {
    Ok(manifest) => {
      Ok(WorkspaceBackupResult {
        manifest,
        reopen_ok,
        reopen_error,
      })
    }
    Err(err) => {
      // Even if snapshot failed, return the error with reopen status
      // This helps diagnose whether the app is still usable
      Err(err)
    }
  }
}
