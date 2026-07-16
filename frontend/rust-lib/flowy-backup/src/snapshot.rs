use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum BackupError {
  #[error("snapshot io: {0}")]
  Io(#[from] std::io::Error),
  #[error("snapshot failed: {0}")]
  Snapshot(String),
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

/// Recursively copies the quiescent collab_db directory to out_dir.
///
/// **PRECONDITION:** Correct only when no CollabKVDB handle is open on collab_db_path.
/// The app-wide quiesce that guarantees this is a caller (Plan 2) concern.
/// Without this precondition, RocksDB background threads may mutate files during the copy,
/// corrupting the snapshot.
///
/// When the handle is dropped, RocksDB flushes the WAL and stops all background threads,
/// making the directory quiescent. This function then performs a byte-for-byte recursive copy.
pub fn snapshot_closed_collab_db(collab_db_path: &Path, out_dir: &Path) -> Result<(), BackupError> {
  // Create the output directory if it doesn't exist
  std::fs::create_dir_all(out_dir)?;

  // Recursively copy all files and subdirectories
  copy_dir_recursive(collab_db_path, out_dir)
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), BackupError> {
  for entry in std::fs::read_dir(src)? {
    let entry = entry?;
    let path = entry.path();
    let file_name = entry.file_name();
    let dest_path = dst.join(&file_name);

    if path.is_dir() {
      std::fs::create_dir_all(&dest_path)?;
      copy_dir_recursive(&path, &dest_path)?;
    } else {
      std::fs::copy(&path, &dest_path)?;
    }
  }
  Ok(())
}
