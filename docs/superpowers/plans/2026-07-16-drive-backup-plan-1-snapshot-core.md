# Drive Backup — Plan 1: WAL Prerequisite + Snapshot Core (with spike)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove AppFlowy's live per-user data dir can be snapshotted to a byte-identical, openable, referentially-consistent copy while the app runs, and land the WAL prerequisite that makes it safe.

**Architecture:** Fix the SQLite WAL pragma so it actually applies, then build a `SnapshotService` that, inside the app's quiesce window, copies `flowy-database.db` via `VACUUM INTO` and `collab_db/` via **Drop-and-Copy** (close the RocksDB handle → copy the quiescent dir → reopen) into a staging dir, and restores it back. This plan stops at the local snapshot↔restore round-trip — no Drive, no UI. It contains the make-or-break spike (drop-and-copy correctness incl. WAL-replay); Plans 2–5 depend on this passing.

**Tech Stack:** Rust, Diesel/rusqlite (`flowy-sqlite`), `collab-plugins` `CollabKVDB` (RocksDB), `collab-integrate`.

## Global Constraints

- Desktop only (macOS / Windows / Linux).
- No new behavior in Local vs Cloud mode beyond snapshotting the local data dir.
- Data dir layout: `{app_data_dir}/{uid}/flowy-database.db`, `{app_data_dir}/{uid}/collab_db/`. Exclude `indexes/` and `cache_files/` from snapshots.
- **collab_db snapshot mechanism = Drop-and-Copy** (decided 2026-07-16 after the first spike disproved live-checkpoint). `collab_db` has no reachable live-snapshot primitive; a consistent copy requires the RocksDB handle to be fully closed (quiescent) first, then byte-copy, then reopen. Copying an open `collab_db` dir is UNSAFE (background compaction tears the SST set). `CollabKVDB::flush()` is a no-op — do not rely on it.
- Cross-DB consistency is guaranteed by a **quiesce window** (writers paused, `collab_db` handle closed) that spans both stores' copies, not by copy ordering.
- **Fork constraint (hard):** this repo is a merge-friendly fork of upstream AppFlowy. Keep new code in the `flowy-backup` crate; do NOT rename upstream symbols; do NOT fork external deps (`collab-plugins`, `rust-rocksdb`). Plan 1 stays entirely within `flowy-backup` plus the one-line WAL fix already landed in Task 0.
- Plan 1 scope note: it proves drop-and-copy correctness given **exclusive ownership of the handle** (the test controls all refs). The app-wide quiesce orchestration (coordinating the ~10 `Weak<CollabKVDB>` holders + pausing editing) is a Plan 2 integration task, not Plan 1.
- Pinned deps: `rocksdb` rev `1710120e4549e04ba3baa6a1ee5a5a801fa45a72`, `collab-plugins` git rev `4dfccef` (see root `Cargo.toml`).

---

### Task 0: WAL prerequisite fix

**Files:**
- Modify: `frontend/rust-lib/flowy-sqlite/src/sqlite_impl/pool.rs:146-154`
- Test: `frontend/rust-lib/flowy-sqlite/src/sqlite_impl/pool.rs` (inline `#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: a guarantee that every pooled connection is in `journal_mode = wal` at runtime. Task 2 relies on this for concurrent-writer-safe `VACUUM INTO`.

- [ ] **Step 1: Write the failing test**

Add to `pool.rs`:

```rust
#[cfg(test)]
mod wal_tests {
  use super::*;
  use crate::sqlite_impl::PoolConfig;

  #[test]
  fn every_acquired_conn_is_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let pool = ConnectionPool::new(PoolConfig::default(), dir.path()).unwrap();
    let conn = pool.get().unwrap();
    let mode: String = conn
      .query_row("PRAGMA journal_mode;", [], |r| r.get(0))
      .unwrap();
    assert_eq!(mode.to_lowercase(), "wal");
  }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flowy-sqlite wal_tests -- --nocapture`
Expected: FAIL — journal mode returns `delete` (pragma never set because default equals WAL and the guard skips it).

- [ ] **Step 3: Write minimal implementation**

In `on_acquire`, make the journal-mode pragma unconditional:

```rust
fn on_acquire(&self, conn: &mut SqliteConnection) -> Result<()> {
  conn.pragma_set_busy_timeout(self.config.busy_timeout)?;
  conn.pragma_set_journal_mode(self.config.journal_mode, None)?;
  conn.pragma_set_synchronous(self.config.synchronous, None)?;
  Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flowy-sqlite wal_tests -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Verify no regression in the sqlite crate**

Run: `cargo test -p flowy-sqlite`
Expected: PASS (all existing tests green).

- [ ] **Step 6: Commit**

```bash
git add frontend/rust-lib/flowy-sqlite/src/sqlite_impl/pool.rs
git commit -m "fix(sqlite): apply journal_mode pragma unconditionally so WAL is actually enabled"
```

> Note: adjust `ConnectionPool::new` / `PoolConfig` names in Step 1 to the crate's actual constructor if they differ; confirm by reading the top of `pool.rs` before writing the test. The behavior asserted (runtime `journal_mode = wal`) is what matters.

---

### Task 1: `flowy-backup` crate skeleton + SnapshotService interface

**Files:**
- Create: `frontend/rust-lib/flowy-backup/Cargo.toml`
- Create: `frontend/rust-lib/flowy-backup/src/lib.rs`
- Create: `frontend/rust-lib/flowy-backup/src/snapshot.rs`
- Modify: `frontend/rust-lib/Cargo.toml` (add `flowy-backup` to workspace `members`)

**Interfaces:**
- Consumes: nothing yet.
- Produces:
  - `struct SnapshotService { data_root: PathBuf }`
  - `SnapshotService::new(data_root: impl Into<PathBuf>) -> Self`
  - `fn snapshot(&self, uid: i64, collab_db: &Arc<CollabKVDB>, sqlite_pool: &ConnectionPool, out_dir: &Path) -> Result<SnapshotManifest, BackupError>` — signature only in this task (returns `unimplemented!()`), filled in Task 2/3.
  - `fn restore(&self, uid: i64, snapshot_dir: &Path) -> Result<(), BackupError>` — signature only, filled in Task 4.
  - `struct SnapshotManifest { pub checksums: BTreeMap<String, String> }`
  - `enum BackupError` (thiserror).

- [ ] **Step 1: Create the crate manifest**

`frontend/rust-lib/flowy-backup/Cargo.toml`:

```toml
[package]
name = "flowy-backup"
version = "0.1.0"
edition = "2021"

[dependencies]
thiserror = { workspace = true }
tracing = { workspace = true }
sha2 = "0.10"
tempfile = { workspace = true }

[dev-dependencies]
tempfile = { workspace = true }
```

- [ ] **Step 2: Define the interface (compiles, unimplemented bodies)**

`frontend/rust-lib/flowy-backup/src/snapshot.rs`:

```rust
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
```

`frontend/rust-lib/flowy-backup/src/lib.rs`:

```rust
pub mod snapshot;
pub use snapshot::{BackupError, SnapshotManifest, SnapshotService};
```

- [ ] **Step 3: Register in the workspace and build**

Add `"flowy-backup"` to the `members` array in `frontend/rust-lib/Cargo.toml`.

Run: `cargo build -p flowy-backup`
Expected: compiles clean.

- [ ] **Step 4: Commit**

```bash
git add frontend/rust-lib/flowy-backup/ frontend/rust-lib/Cargo.toml
git commit -m "feat(backup): scaffold flowy-backup crate and SnapshotService interface"
```

---

### Task 2: SPIKE — Drop-and-Copy snapshot of `collab_db` (the gate)

**This task proves the snapshot mechanism is correct. Do not proceed to Plan 2+ until Step 5 passes with a REALISTIC test (not the hollow pattern the first attempt failed on).**

Decision already made (see Global Constraints): live-checkpoint is unreachable; the mechanism is **Drop-and-Copy** — the `collab_db` RocksDB handle must be fully dropped (closed) so RocksDB syncs its WAL and stops all background threads, THEN the quiescent directory is byte-copied, THEN reopened. A reopened copy replays the WAL, so committed data survives. There is no concurrent writer during the copy by construction (the handle is closed), which is exactly why it's safe.

**Files:**
- Modify: `frontend/rust-lib/flowy-backup/src/snapshot.rs`
- Modify: `frontend/rust-lib/flowy-backup/Cargo.toml` (add `collab-integrate` — which re-exports `CollabKVDB` — and any needed collab crates; match workspace dep conventions)
- Test: `frontend/rust-lib/flowy-backup/tests/collab_db_snapshot_spike.rs`

**Interfaces:**
- Consumes: `CollabKVDB` from `collab_integrate` (`use collab_integrate::CollabKVDB;`), opened via `CollabKVDB::open(&path)`; write via `db.with_write_txn(|txn| ...)`; read via `db.read_txn()` (see `flowy-user/src/services/db.rs` and `flowy-user/src/migrations/*.rs` for exact `KVStore`/txn method shapes).
- Produces: `fn snapshot_closed_collab_db(collab_db_path: &Path, out_dir: &Path) -> Result<(), BackupError>` — a recursive directory copy that is **only correct when no `CollabKVDB` handle is open on `collab_db_path`**. Its doc comment MUST state that precondition (the app-wide quiesce that guarantees it is a Plan 2 concern).

- [ ] **Step 1: Add deps and write the REALISTIC failing spike test**

The test must avoid the two flaws that sank the first attempt: (a) it must NOT keep a handle open during the copy — it drops it; (b) it must write enough data that a reopened copy genuinely exercises WAL replay / on-disk state, and it must assert that the **last write committed immediately before the drop** is present in the copy.

`frontend/rust-lib/flowy-backup/tests/collab_db_snapshot_spike.rs`:

```rust
use collab_integrate::CollabKVDB;
use flowy_backup::snapshot::snapshot_closed_collab_db;
use std::sync::Arc;

// Helper write/read using the real KVStore txn API (fill exact method names
// from flowy-user/src/services/db.rs + migrations/*.rs during implementation).
fn write_doc(db: &Arc<CollabKVDB>, uid: i64, object_id: &str, bytes: &[u8]) { /* with_write_txn insert */ }
fn read_doc(db: &Arc<CollabKVDB>, uid: i64, object_id: &str) -> Option<Vec<u8>> { /* read_txn get */ }

#[test]
fn dropped_collab_db_copies_and_reopens_identically() {
  let root = tempfile::tempdir().unwrap();
  let src = root.path().join("collab_db");
  let out = root.path().join("collab_db_copy");
  let uid = 1;

  // 1. open, write MANY docs so real SST/WAL state exists (e.g. 2_000 docs of ~1KB)
  let db = Arc::new(CollabKVDB::open(&src).unwrap());
  for i in 0..2_000 { write_doc(&db, uid, &format!("obj-{i}"), &vec![(i % 251) as u8; 1024]); }
  // 2. the CRITICAL last write, committed right before the drop:
  write_doc(&db, uid, "obj-final", b"the-last-committed-bytes");

  // 3. DROP all handles -> RocksDB closes, syncs WAL, stops background threads
  assert_eq!(Arc::strong_count(&db), 1, "test must hold the only ref before drop");
  drop(db);

  // 4. copy the now-quiescent dir
  snapshot_closed_collab_db(&src, &out).unwrap();

  // 5. reopen the COPY and assert both an early doc and the last-committed doc survive
  let copy = Arc::new(CollabKVDB::open(&out).unwrap());
  assert_eq!(read_doc(&copy, uid, "obj-0").as_deref(), Some(&vec![0u8; 1024][..]));
  assert_eq!(read_doc(&copy, uid, "obj-final").as_deref(), Some(&b"the-last-committed-bytes"[..]));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flowy-backup --test collab_db_snapshot_spike`
Expected: FAIL — `snapshot_closed_collab_db` not implemented (and helper bodies todo).

- [ ] **Step 3: Implement `snapshot_closed_collab_db` + fill the helpers**

Implement a recursive directory copy (create `out_dir`, copy every file/subdir from `collab_db_path`). Fill `write_doc`/`read_doc` with the real `CollabKVDB` transaction API discovered from `flowy-user/src/services/db.rs` and `migrations/*.rs`. Add a doc comment on `snapshot_closed_collab_db` stating the "no open handle" precondition.

- [ ] **Step 4: Run the spike to verify it passes**

Run: `cargo test -p flowy-backup --test collab_db_snapshot_spike -- --nocapture`
Expected: PASS — the dropped-then-copied DB reopens with both `obj-0` and `obj-final` intact (proves WAL/state fully persisted on drop and the copy is complete).

- [ ] **Step 5: GATE + commit**

If PASS, commit:

```bash
git add frontend/rust-lib/flowy-backup/
git commit -m "spike(backup): prove drop-and-copy of closed collab_db reopens with all committed data"
```

If the last-committed doc is MISSING after reopen, drop-and-copy on a bare drop is insufficient (WAL not synced on drop) — STOP and escalate: the next option is an explicit close/flush API or forcing a WAL sync before drop. Do not paper over it. Max 3 attempts, then escalate.

---

### Task 3: SQLite `VACUUM INTO` + full snapshot assembly

**Files:**
- Modify: `frontend/rust-lib/flowy-backup/src/snapshot.rs`
- Test: `frontend/rust-lib/flowy-backup/tests/snapshot_roundtrip.rs`

**Interfaces:**
- Consumes: `snapshot_closed_collab_db` (Task 2), a SQLite connection source.
- Produces: `fn snapshot_sqlite(db_path: &Path, out_path: &Path) -> Result<(), BackupError>` and the full `SnapshotService::snapshot(...)` body that (a) assumes it runs inside the app's quiesce window with the `collab_db` handle already closed, (b) VACUUM INTOs SQLite, (c) copies the closed `collab_db` via `snapshot_closed_collab_db`, (d) computes sha256 checksums, (e) returns `SnapshotManifest`.

- [ ] **Step 1: Write the failing test**

`snapshot_roundtrip.rs`:

```rust
#[test]
fn vacuum_into_produces_openable_consistent_db() {
  let dir = tempfile::tempdir().unwrap();
  let src = dir.path().join("flowy-database.db");
  // create WAL db, insert row (id=1,name='a'), leave uncheckpointed in WAL
  seed_wal_db(&src, 1, "a");
  let out = dir.path().join("snap.db");
  flowy_backup::snapshot::snapshot_sqlite(&src, &out).unwrap();
  // open `out` fresh, assert row (1,'a') present
  assert_eq!(read_name(&out, 1), "a");
}
```

(Include `seed_wal_db` / `read_name` helpers using `rusqlite` directly in the test file.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p flowy-backup --test snapshot_roundtrip`
Expected: FAIL — `snapshot_sqlite` not implemented.

- [ ] **Step 3: Implement `snapshot_sqlite`**

```rust
pub fn snapshot_sqlite(db_path: &Path, out_path: &Path) -> Result<(), BackupError> {
  let conn = rusqlite::Connection::open(db_path)
    .map_err(|e| BackupError::Snapshot(e.to_string()))?;
  let sql = format!("VACUUM INTO '{}'", out_path.to_string_lossy().replace('\'', "''"));
  conn.execute_batch(&sql).map_err(|e| BackupError::Snapshot(e.to_string()))?;
  Ok(())
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p flowy-backup --test snapshot_roundtrip`
Expected: PASS — WAL-resident row is captured in the vacuumed copy.

- [ ] **Step 5: Implement `SnapshotService::snapshot` (both DBs + checksums)**

Assemble `snapshot_sqlite` (VACUUM INTO) + `snapshot_closed_collab_db` into a staging dir, sha256 each output, return `SnapshotManifest`. This crate stays free of app-global locks: the app-wide quiesce (pausing writers + closing the `collab_db` handle) is the caller's responsibility (BackupManager / Plan 2). Document the precondition on `snapshot` in its doc comment: **callers must have paused SQLite writers and closed the `collab_db` handle before calling.**

- [ ] **Step 6: Commit**

```bash
git add frontend/rust-lib/flowy-backup/
git commit -m "feat(backup): snapshot sqlite via VACUUM INTO and assemble full snapshot with checksums"
```

---

### Task 4: Local restore round-trip (byte-identical proof)

**Files:**
- Modify: `frontend/rust-lib/flowy-backup/src/snapshot.rs`
- Test: `frontend/rust-lib/flowy-backup/tests/full_roundtrip.rs`

**Interfaces:**
- Consumes: `SnapshotService::snapshot`.
- Produces: `SnapshotService::restore(uid, snapshot_dir)` that unzips/copies a snapshot into a fresh target dir (staging semantics only — the launch-time swap is Plan 2).

- [ ] **Step 1: Write the failing integration test**

`full_roundtrip.rs`:

```rust
#[test]
fn backup_then_restore_is_byte_identical() {
  // 1. build a fake user dir: seed flowy-database.db + a real CollabKVDB with a doc
  // 2. SnapshotService::snapshot(...) -> manifest, staging dir
  // 3. restore into a NEW empty target dir
  // 4. assert sqlite row equal AND collab doc bytes equal AND checksums match manifest
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p flowy-backup --test full_roundtrip`
Expected: FAIL — `restore` unimplemented.

- [ ] **Step 3: Implement `restore`**

Copy the snapshot's `flowy-database.db` and `collab_db/` into the target dir, re-verify sha256 against the manifest, error on mismatch.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p flowy-backup --test full_roundtrip`
Expected: PASS — restored SQLite row and collab doc are byte-identical; checksums match.

- [ ] **Step 5: Full crate test + commit**

```bash
cargo test -p flowy-backup
git add frontend/rust-lib/flowy-backup/
git commit -m "feat(backup): restore snapshot into target dir with checksum verification; full round-trip proven"
```

---

## Self-Review

- **Spec coverage (this plan's slice):** WAL prereq (Task 0) ✓; snapshot core / VACUUM INTO + collab_db drop-and-copy in the quiesce window (Tasks 2–3) ✓; checksum verification (Tasks 3–4) ✓; local restore round-trip (Task 4) ✓. Deferred to later plans (explicitly out of Plan 1 scope): app-wide quiesce orchestration (closing the ~10 `Weak<CollabKVDB>` holders + pausing editing), Drive client/OAuth/manifest-last, staged restore-on-launch boot path, 2a guardrail persistence, retention/backoff, Flutter UI. These are named in the spec's Implementation Sequencing §2–6.
- **Placeholder scan:** the only intentional `todo!`/discovery points are in the Task 2 spike, which is by nature an investigation; every other step carries real code or a real command. Task 0 Step 1 flags that constructor names must be confirmed against `pool.rs`.
- **Type consistency:** `SnapshotService`, `SnapshotManifest`, `BackupError`, `snapshot_closed_collab_db`, `snapshot_sqlite`, `snapshot`, `restore` names are used consistently across Tasks 1–4.

## Gate

Task 2 (spike) is a hard gate. If it fails and cannot be resolved in 3 attempts, stop and revisit the spec's approach before writing Plans 2–5.
