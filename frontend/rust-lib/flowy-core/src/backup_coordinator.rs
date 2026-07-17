use std::sync::Weak;
use tokio::sync::RwLock;
use once_cell::sync::Lazy;
use flowy_error::FlowyResult;
use std::path::Path;
use flowy_backup::SnapshotManifest;

/// Global backup coordinator - stores a weak reference to AppFlowyCore
/// This allows event handlers to access the backup functionality
static BACKUP_COORDINATOR: Lazy<RwLock<Option<Weak<crate::AppFlowyCore>>>> =
  Lazy::new(|| RwLock::new(None));

/// Set the AppFlowyCore reference for backup operations
pub async fn set_app_flowy_core(core: Weak<crate::AppFlowyCore>) {
  let mut coordinator = BACKUP_COORDINATOR.write().await;
  *coordinator = Some(core);
}

/// Execute a workspace backup via the global coordinator
pub async fn run_workspace_backup(staging_dir: &Path) -> FlowyResult<SnapshotManifest> {
  let coordinator = BACKUP_COORDINATOR.read().await;
  let weak_core = coordinator
    .as_ref()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("AppFlowyCore not initialized for backup"))?;

  let core = weak_core
    .upgrade()
    .ok_or_else(|| flowy_error::FlowyError::internal()
      .with_context("AppFlowyCore reference no longer valid"))?;

  core.run_workspace_backup(staging_dir).await
}
