use collab_integrate::CollabKVDB;
use collab_plugins::local_storage::kv::{KVStore, KVTransactionDB};
use flowy_backup::snapshot::snapshot_closed_collab_db;
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

#[test]
fn dropped_collab_db_copies_and_reopens_identically() {
  let root = tempfile::tempdir().expect("Failed to create temp dir");
  let src = root.path().join("collab_db");
  let out = root.path().join("collab_db_copy");

  // 1. Open, write MANY docs so real SST/WAL state exists (e.g. 2_000 docs of ~1KB)
  let db = Arc::new(CollabKVDB::open(&src).expect("Failed to open CollabKVDB"));

  for i in 0..2_000 {
    let key = format!("obj-{}", i);
    let value = vec![(i % 251) as u8; 1024];
    write_kv(&db, &key, &value);
  }

  // 2. The CRITICAL last write, committed right before the drop:
  write_kv(&db, "obj-final", b"the-last-committed-bytes");

  // 3. DROP all handles -> RocksDB closes, syncs WAL, stops background threads
  assert_eq!(
    Arc::strong_count(&db),
    1,
    "test must hold the only ref before drop"
  );
  drop(db);

  // 4. Copy the now-quiescent dir
  snapshot_closed_collab_db(&src, &out).expect("Failed to snapshot");

  // 5. Reopen the COPY and assert both an early doc and the last-committed doc survive
  let copy = Arc::new(CollabKVDB::open(&out).expect("Failed to reopen copy"));

  // Check an early entry
  let obj_0_value = read_kv(&copy, "obj-0");
  assert_eq!(obj_0_value.as_deref(), Some(&vec![0u8; 1024][..]));

  // Check the final critical entry (proves WAL was synced on drop)
  let obj_final_value = read_kv(&copy, "obj-final");
  assert_eq!(
    obj_final_value.as_deref(),
    Some(&b"the-last-committed-bytes"[..])
  );
}
