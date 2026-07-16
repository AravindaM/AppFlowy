use std::collections::BTreeMap;
use std::path::PathBuf;

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
