use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use tempfile::tempdir;

use collab_plugins::CollabKVDB;
use collab_plugins::local_storage::kv::{KVTransactionDB, KVStore};
use flowy_backup::snapshot::checkpoint_collab_db_with_db;

#[test]
fn checkpoint_of_live_collab_db_restores_identically() {
    let src_dir = tempdir().expect("create src temp dir");
    let out_dir = tempdir().expect("create out temp dir");

    let src_path = src_dir.path();
    let out_path = out_dir.path();

    // 1. Open source CollabKVDB
    let db = CollabKVDB::open(&src_path).expect("open source db");
    let db = Arc::new(db);

    // 2. Write a known key-value pair using the raw KVStore API
    let known_key = b"test-key-1";
    let known_value = b"test data for checkpoint spike";

    db.with_write_txn(|txn| {
        txn.insert(known_key, known_value)
            .expect("insert test data");
        Ok(())
    })
    .expect("write known data");

    // Verify the data was written
    {
        let read_txn = db.read_txn();
        let read_value = read_txn.get(known_key).expect("get should succeed");
        assert!(read_value.is_some(), "known key should exist after write");
    }

    // 3. Spawn a writer thread that continuously writes to different keys
    let db_clone = db.clone();
    let stop_flag = Arc::new(AtomicBool::new(false));
    let stop_flag_clone = stop_flag.clone();

    let writer_thread = thread::spawn(move || {
        let mut counter = 0;
        while !stop_flag_clone.load(Ordering::Relaxed) {
            let churn_key = format!("churn-key-{}", counter);
            let churn_data = format!("churn-data-{}", counter);
            let _ = db_clone.with_write_txn(|txn| {
                txn.insert(churn_key.as_bytes(), churn_data.as_bytes())
                    .expect("insert churn data");
                Ok(())
            });
            counter += 1;
            thread::sleep(Duration::from_millis(1));
        }
    });

    // 4. Give the writer a moment to start, then checkpoint
    thread::sleep(Duration::from_millis(10));

    checkpoint_collab_db_with_db(&db, &src_path, &out_path).expect("checkpoint should succeed");

    // 5. Stop the writer thread
    stop_flag.store(true, Ordering::Relaxed);
    writer_thread.join().expect("join writer thread");

    // 6. Open the checkpointed DB and verify the known data
    let checkpointed_db = CollabKVDB::open(&out_path).expect("open checkpointed db");
    let read_txn = checkpointed_db.read_txn();
    let read_data = read_txn
        .get(known_key)
        .expect("get should succeed");

    assert!(
        read_data.is_some(),
        "known key should exist in checkpointed db"
    );

    let read_bytes = read_data.unwrap();
    assert_eq!(
        &read_bytes[..], known_value,
        "checkpointed data should match original"
    );
}
