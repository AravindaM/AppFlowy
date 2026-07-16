use flowy_backup::snapshot::snapshot_sqlite;
use rusqlite::{Connection, Result as SqliteResult};
use std::path::Path;

/// Create a WAL-mode SQLite database and seed it with a row, leaving data in the WAL
/// (not checkpointed to the main database file).
fn seed_wal_db(db_path: &Path, id: i32, name: &str) -> SqliteResult<()> {
  let conn = Connection::open(db_path)?;
  conn.execute_batch("PRAGMA journal_mode = WAL")?;
  conn.execute_batch("CREATE TABLE test (id INTEGER PRIMARY KEY, name TEXT)")?;
  conn.execute("INSERT INTO test (id, name) VALUES (?1, ?2)", [&id.to_string(), &name.to_string()])?;

  // Do NOT checkpoint: leave the data in the WAL
  Ok(())
}

/// Read a name from the test table by id.
fn read_name(db_path: &Path, id: i32) -> String {
  let conn = Connection::open(db_path).expect("Failed to open database");
  let mut stmt = conn.prepare("SELECT name FROM test WHERE id = ?1").expect("Failed to prepare");
  let name: String = stmt
    .query_row([id], |row| row.get(0))
    .expect("Failed to query row");
  name
}

#[test]
fn vacuum_into_produces_openable_consistent_db() {
  let dir = tempfile::tempdir().unwrap();
  let src = dir.path().join("flowy-database.db");
  // create WAL db, insert row (id=1,name='a'), leave uncheckpointed in WAL
  seed_wal_db(&src, 1, "a").unwrap();
  let out = dir.path().join("snap.db");
  snapshot_sqlite(&src, &out).unwrap();
  // open `out` fresh, assert row (1,'a') present
  assert_eq!(read_name(&out, 1), "a");
}
