use collab_integrate::CollabKVDB;
use collab_plugins::local_storage::kv::{KVStore, KVTransactionDB};
use flowy_backup::snapshot::{SnapshotService, BackupError};
use rusqlite::{Connection, Result as SqliteResult};
use std::sync::Arc;

/// Helper to write raw key-value data using the real KVStore transaction API.
fn write_kv(db: &Arc<CollabKVDB>, key: &str, value: &[u8]) {
  db.with_write_txn(|txn| {
    txn.insert(key.as_bytes(), value)?;
    Ok(())
  })
  .expect("Failed to write");
}

/// Helper to read raw key-value data using the real KVStore transaction API.
fn read_kv(db: &Arc<CollabKVDB>, key: &str) -> Option<Vec<u8>> {
  let read_txn = db.read_txn();
  if let Ok(Some(v)) = read_txn.get(key.as_bytes()) {
    let bytes: &[u8] = v.as_ref();
    Some(bytes.to_vec())
  } else {
    None
  }
}

/// Create a WAL-mode SQLite database and seed it with a row.
fn seed_wal_db(db_path: &std::path::Path, id: i32, name: &str) -> SqliteResult<()> {
  let conn = Connection::open(db_path)?;
  conn.execute_batch("PRAGMA journal_mode = WAL")?;
  conn.execute_batch("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")?;
  conn.execute("INSERT INTO test (id, name) VALUES (?1, ?2)", [&id.to_string(), &name.to_string()])?;
  Ok(())
}

/// Read a name from the test table by id.
fn read_name(db_path: &std::path::Path, id: i32) -> String {
  let conn = Connection::open(db_path).expect("Failed to open database");
  let mut stmt = conn.prepare("SELECT name FROM test WHERE id = ?1").expect("Failed to prepare");
  let name: String = stmt
    .query_row([id], |row| row.get(0))
    .expect("Failed to query row");
  name
}

#[test]
fn backup_then_restore_is_byte_identical() {
  let root = tempfile::tempdir().expect("Failed to create temp dir");

  // 1. Build a fake user dir with both flowy-database.db and collab_db
  let user_dir = root.path().join("user");
  std::fs::create_dir_all(&user_dir).expect("Failed to create user dir");

  // Create a SQLite database with a row
  let sqlite_path = user_dir.join("flowy-database.db");
  seed_wal_db(&sqlite_path, 1, "test-value").expect("Failed to seed SQLite db");

  // Create a CollabKVDB with a known entry
  let collab_db_path = user_dir.join("collab_db");
  let db = Arc::new(CollabKVDB::open(&collab_db_path).expect("Failed to open CollabKVDB"));
  write_kv(&db, "test-key", b"test-collab-value");
  drop(db);  // Close the handle so collab_db is quiescent

  // 2. Snapshot the user dir
  let staging_dir = root.path().join("staging");
  let service = SnapshotService::new(root.path());
  let manifest = service.snapshot(&user_dir, &staging_dir)
    .expect("Failed to snapshot");

  // Verify staging directory checksum is consistent
  let staging_collab_checksum1 = flowy_backup::snapshot::compute_sha256_dir(
    &staging_dir.join("collab_db")
  ).expect("Failed to compute staging checksum 1");
  eprintln!("Staging checksum (before restore): {:?}", staging_collab_checksum1);

  // 3. Restore into a new empty target dir
  let target_dir = root.path().join("target");
  std::fs::create_dir_all(&target_dir).expect("Failed to create target dir");

  service.restore(&staging_dir, &target_dir, &manifest)
    .expect("Failed to restore");

  // Verify staging directory checksum is still the same
  let staging_collab_checksum2 = flowy_backup::snapshot::compute_sha256_dir(
    &staging_dir.join("collab_db")
  ).expect("Failed to compute staging checksum 2");
  eprintln!("Staging checksum (after restore): {:?}", staging_collab_checksum2);
  assert_eq!(staging_collab_checksum1, staging_collab_checksum2, "Staging checksum should not change");

  // 4. Verify checksums BEFORE opening CollabKVDB (which may modify the directory)
  let restored_sqlite = target_dir.join("flowy-database.db");
  assert!(restored_sqlite.exists(), "Restored SQLite db should exist");

  let restored_collab_db = target_dir.join("collab_db");
  assert!(restored_collab_db.exists(), "Restored collab_db should exist");

  // Verify sqlite and collab_db checksums match
  let restored_sqlite_checksum = flowy_backup::snapshot::compute_sha256_file(&restored_sqlite)
    .expect("Failed to compute restored sqlite checksum");
  let expected_sqlite = manifest.checksums.get("flowy-database.db").expect("Missing sqlite checksum in manifest");
  assert_eq!(&restored_sqlite_checksum, expected_sqlite, "SQLite checksum should match");

  let restored_collab_checksum = flowy_backup::snapshot::compute_sha256_dir(&restored_collab_db)
    .expect("Failed to compute restored collab checksum");
  let expected_collab = manifest.checksums.get("collab_db").expect("Missing collab checksum in manifest");
  assert_eq!(&restored_collab_checksum, expected_collab, "collab_db checksum should match");

  // 5. NOW open CollabKVDB and verify data integrity
  let copy = Arc::new(CollabKVDB::open(&restored_collab_db)
    .expect("Failed to open restored CollabKVDB"));
  let collab_value = read_kv(&copy, "test-key");
  assert_eq!(collab_value.as_deref(), Some(&b"test-collab-value"[..]), "collab_db entry should match");
  drop(copy);

  // 6. Verify SQLite row is identical
  assert_eq!(read_name(&restored_sqlite, 1), "test-value", "SQLite row should match");
}

#[test]
fn restore_fails_on_checksum_mismatch() {
  let root = tempfile::tempdir().expect("Failed to create temp dir");

  // 1. Build a fake user dir
  let user_dir = root.path().join("user");
  std::fs::create_dir_all(&user_dir).expect("Failed to create user dir");

  let sqlite_path = user_dir.join("flowy-database.db");
  seed_wal_db(&sqlite_path, 1, "test-value").expect("Failed to seed SQLite db");

  let collab_db_path = user_dir.join("collab_db");
  let db = Arc::new(CollabKVDB::open(&collab_db_path).expect("Failed to open CollabKVDB"));
  write_kv(&db, "test-key", b"test-collab-value");
  drop(db);

  // 2. Snapshot
  let staging_dir = root.path().join("staging");
  let service = SnapshotService::new(root.path());
  let mut manifest = service.snapshot(&user_dir, &staging_dir)
    .expect("Failed to snapshot");

  // 3. Corrupt the manifest by changing a checksum
  manifest.checksums.insert("flowy-database.db".to_string(), "corrupted-checksum".to_string());

  // 4. Attempt restore - should fail
  let target_dir = root.path().join("target");
  std::fs::create_dir_all(&target_dir).expect("Failed to create target dir");

  let result = service.restore(&staging_dir, &target_dir, &manifest);
  assert!(result.is_err(), "restore should fail on checksum mismatch");

  // Verify the error message mentions checksum
  match result {
    Err(BackupError::Snapshot(msg)) => {
      assert!(msg.contains("checksum"), "Error message should mention checksum");
    },
    _ => panic!("Expected BackupError::Snapshot with checksum error"),
  }
}
