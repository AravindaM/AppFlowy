use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use anyhow;

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
  #[error("snapshot io: {0}")]
  Io(#[from] std::io::Error),
  #[error("snapshot failed: {0}")]
  Snapshot(String),
  #[error("checkpoint error: {0}")]
  Checkpoint(String),
}

#[derive(Debug, Clone)]
pub struct SnapshotManifest {
  pub checksums: BTreeMap<String, String>,
}

pub struct SnapshotService {
  data_root: PathBuf,
}

impl SnapshotService {
  pub fn new(data_root: impl Into<PathBuf>) -> Self {
    Self { data_root: data_root.into() }
  }

  pub fn user_dir(&self, uid: i64) -> PathBuf {
    self.data_root.join(uid.to_string())
  }
}

/// Creates a checkpoint (snapshot) of a live CollabKVDB using a quiesce-and-copy approach.
///
/// This function creates a physical checkpoint of the RocksDB database using these steps:
/// 1. Take an exclusive write transaction on the open database
/// 2. Flush any pending writes
/// 3. Copy the entire database directory while holding the transaction
/// 4. Release the transaction
///
/// This ensures consistency because no writes can occur during the copy.
///
/// # Arguments
/// * `db` - The open CollabKVDB database
/// * `collab_db_path` - Path to the live CollabKVDB database directory
/// * `out_dir` - Directory where the checkpoint will be created
///
/// # Returns
/// * `Ok(())` if the checkpoint was created successfully
/// * `Err(BackupError)` if the checkpoint failed
pub fn checkpoint_collab_db_with_db(
  db: &collab_plugins::CollabKVDB,
  collab_db_path: &Path,
  out_dir: &Path,
) -> Result<(), BackupError> {
  use collab_plugins::local_storage::kv::KVTransactionDB;
  use collab_plugins::local_storage::kv::error::PersistenceError;

  // Quiesce: Take an exclusive write transaction to ensure consistency
  // This prevents any other writes from occurring during the copy
  db.with_write_txn(|_txn| {
    // Flush any pending writes
    db.flush()
      .map_err(|e| PersistenceError::Internal(anyhow::anyhow!("failed to flush db: {:?}", e)))?;

    // Copy the database directory while the transaction is held
    copy_dir_all(collab_db_path, out_dir)
      .map_err(|e| {
        PersistenceError::Internal(
          anyhow::anyhow!("failed to copy database: {:?}", e)
        )
      })?;

    Ok(())
  })
  .map_err(|e| BackupError::Checkpoint(format!("transaction error: {:?}", e)))?;

  Ok(())
}

/// Creates a checkpoint (snapshot) of a live CollabKVDB.
///
/// This is a convenience wrapper that opens the database and calls `checkpoint_collab_db_with_db`.
///
/// # Arguments
/// * `collab_db_path` - Path to the live CollabKVDB database
/// * `out_dir` - Directory where the checkpoint will be created
///
/// # Returns
/// * `Ok(())` if the checkpoint was created successfully
/// * `Err(BackupError)` if the checkpoint failed
pub fn checkpoint_collab_db(
  collab_db_path: &Path,
  out_dir: &Path,
) -> Result<(), BackupError> {
  use collab_plugins::local_storage::rocksdb::kv_impl::KVTransactionDBRocksdbImpl;
  use collab_plugins::local_storage::kv::KVTransactionDB;
  use collab_plugins::local_storage::kv::error::PersistenceError;

  // Open the database
  let db = KVTransactionDBRocksdbImpl::open(collab_db_path)
    .map_err(|e| BackupError::Checkpoint(format!("failed to open collab db: {:?}", e)))?;

  // Quiesce: Take an exclusive write transaction to ensure consistency
  // This prevents any other writes from occurring during the copy
  db.with_write_txn(|_txn| {
    // Flush any pending writes
    db.flush()
      .map_err(|e| PersistenceError::Internal(anyhow::anyhow!("failed to flush db: {:?}", e)))?;

    // Copy the database directory while the transaction is held
    copy_dir_all(collab_db_path, out_dir)
      .map_err(|e| {
        PersistenceError::Internal(
          anyhow::anyhow!("failed to copy database: {:?}", e)
        )
      })?;

    Ok(())
  })
  .map_err(|e| BackupError::Checkpoint(format!("transaction error: {:?}", e)))?;

  Ok(())
}

/// Recursively copies a directory from `src` to `dst`.
/// Creates `dst` if it doesn't exist.
fn copy_dir_all(src: &Path, dst: &Path) -> std::io::Result<()> {
  std::fs::create_dir_all(dst)?;

  for entry in std::fs::read_dir(src)? {
    let entry = entry?;
    let path = entry.path();
    let file_name = entry.file_name();
    let dest_path = dst.join(&file_name);

    if path.is_dir() {
      copy_dir_all(&path, &dest_path)?;
    } else {
      std::fs::copy(&path, &dest_path)?;
    }
  }

  Ok(())
}
