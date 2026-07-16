use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use rusqlite::Connection;
use sha2::{Sha256, Digest};

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

  /// Assembles a full snapshot by copying both the SQLite database and collab_db,
  /// then computing checksums for each output.
  ///
  /// Returns a `SnapshotManifest` containing sha256 checksums for all copied files.
  ///
  /// **PRECONDITION:** Callers must have paused SQLite writers and closed the
  /// `collab_db` handle before calling this function. The app-wide quiesce that
  /// guarantees this is a caller (Plan 2) concern. Without these preconditions:
  /// - The SQLite database may have concurrent writers, causing `VACUUM INTO` to fail
  /// - RocksDB background threads may mutate collab_db files during the copy, corrupting the snapshot
  ///
  /// # Arguments
  /// * `user_dir` - Source user directory containing `flowy-database.db` and `collab_db/`
  /// * `staging_dir` - Target directory where the snapshot will be assembled
  ///
  /// # Returns
  /// A `SnapshotManifest` with checksums for each copied output, or an error.
  pub fn snapshot(&self, user_dir: &Path, staging_dir: &Path) -> Result<SnapshotManifest, BackupError> {
    std::fs::create_dir_all(staging_dir)?;

    let mut checksums = BTreeMap::new();

    // Snapshot the SQLite database
    let src_sqlite = user_dir.join("flowy-database.db");
    let out_sqlite = staging_dir.join("flowy-database.db");
    snapshot_sqlite(&src_sqlite, &out_sqlite)?;
    let sqlite_checksum = compute_sha256_file(&out_sqlite)?;
    checksums.insert("flowy-database.db".to_string(), sqlite_checksum);

    // Snapshot the collab_db directory
    let src_collab_db = user_dir.join("collab_db");
    let out_collab_db = staging_dir.join("collab_db");
    snapshot_closed_collab_db(&src_collab_db, &out_collab_db)?;
    let collab_db_checksum = compute_sha256_dir(&out_collab_db)?;
    checksums.insert("collab_db".to_string(), collab_db_checksum);

    Ok(SnapshotManifest { checksums })
  }
}

/// Snapshots a SQLite database by running `VACUUM INTO`.
///
/// This creates a consistent copy of the database file, including any data
/// currently in the WAL (Write-Ahead Log) that has not yet been checkpointed
/// to the main database file. The `VACUUM INTO` command performs an atomic
/// copy operation that merges WAL-resident data into the output file.
///
/// **PRECONDITION:** The database at `db_path` must not be open for writing
/// during this operation. SQLite will acquire an exclusive lock, which will
/// fail if another process or thread holds an active connection.
pub fn snapshot_sqlite(db_path: &Path, out_path: &Path) -> Result<(), BackupError> {
  let conn = Connection::open(db_path)
    .map_err(|e| BackupError::Snapshot(e.to_string()))?;

  // Escape single quotes in the output path for the SQL statement
  let escaped_path = out_path.to_string_lossy().replace('\'', "''");
  let sql = format!("VACUUM INTO '{}'", escaped_path);

  conn.execute_batch(&sql)
    .map_err(|e| BackupError::Snapshot(e.to_string()))?;

  Ok(())
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

/// Computes the SHA256 checksum of a file.
fn compute_sha256_file(path: &Path) -> Result<String, BackupError> {
  let contents = std::fs::read(path)?;
  let mut hasher = Sha256::new();
  hasher.update(&contents);
  let result = hasher.finalize();
  Ok(format!("{:x}", result))
}

/// Computes the SHA256 checksum of a directory by hashing all files in sorted order.
/// This ensures consistent checksums across snapshots of the same data.
fn compute_sha256_dir(path: &Path) -> Result<String, BackupError> {
  let mut hasher = Sha256::new();
  let mut entries: Vec<_> = std::fs::read_dir(path)?
    .collect::<Result<Vec<_>, _>>()?;
  entries.sort_by_key(|e| e.path());

  for entry in entries {
    let path = entry.path();
    if path.is_file() {
      let contents = std::fs::read(&path)?;
      hasher.update(&contents);
    } else if path.is_dir() {
      let dir_checksum = compute_sha256_dir(&path)?;
      hasher.update(dir_checksum.as_bytes());
    }
  }

  let result = hasher.finalize();
  Ok(format!("{:x}", result))
}
