# Drive Backup — Plan 1: WAL Prerequisite + Snapshot Core (with spike)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Prove AppFlowy's live per-user data dir can be snapshotted to a byte-identical, openable, referentially-consistent copy while the app runs, and land the WAL prerequisite that makes it safe.

**Architecture:** Fix the SQLite WAL pragma so it actually applies, then build a `SnapshotService` that, under a short write barrier, copies `flowy-database.db` via `VACUUM INTO` and `collab_db/` via a RocksDB checkpoint into a staging dir, and restores it back. This plan stops at the local snapshot↔restore round-trip — no Drive, no UI. It contains the make-or-break spike (RocksDB checkpoint reachability); Plans 2–5 depend on this passing.

**Tech Stack:** Rust, Diesel/rusqlite (`flowy-sqlite`), `collab-plugins` `CollabKVDB` (RocksDB), `collab-integrate`.

## Global Constraints

- Desktop only (macOS / Windows / Linux).
- No new behavior in Local vs Cloud mode beyond snapshotting the local data dir.
- Data dir layout: `{app_data_dir}/{uid}/flowy-database.db`, `{app_data_dir}/{uid}/collab_db/`. Exclude `indexes/` and `cache_files/` from snapshots.
- Cross-DB consistency is guaranteed by a **write barrier**, not by copy ordering.
- No fabricated APIs: Task 2 Step 1 is a discovery gate — the exact RocksDB checkpoint call is found in the pinned `collab-plugins` source before any snapshot code is written against it.
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

### Task 2: SPIKE — RocksDB checkpoint of a live `collab_db` (the gate)

**This task decides whether the whole directory-snapshot approach is viable. Do not proceed to Plan 2+ until Step 6 passes.**

**Files:**
- Modify: `frontend/rust-lib/flowy-backup/src/snapshot.rs`
- Modify: `frontend/rust-lib/flowy-backup/Cargo.toml` (add `collab-integrate`, `collab-plugins` deps once the API is known)
- Test: `frontend/rust-lib/flowy-backup/tests/checkpoint_spike.rs`

**Interfaces:**
- Consumes: `CollabKVDB` from `collab-integrate` / `collab-plugins`.
- Produces: `fn checkpoint_collab_db(collab_db_path: &Path, out_dir: &Path) -> Result<(), BackupError>`.

- [ ] **Step 1: DISCOVERY (no code yet) — find the checkpoint API**

Read the pinned `collab-plugins` source to answer, in writing, in the commit message of Step 5:
1. Does `CollabKVDB` (or the underlying `KVTransactionDB` / RocksDB store) expose a checkpoint / backup / flush API? Names and signatures.
2. Does the pinned `rust-rocksdb` rev expose `rocksdb::checkpoint::Checkpoint`? (Check the vendored/pinned crate.)
3. Can a checkpoint be taken from the **live, open** handle the app holds, or does it require a fresh handle on the dir (RocksDB single-writer lock)?

Commands:
```bash
cargo tree -p flowy-backup -i collab-plugins   # confirm resolution
find ~/.cargo -path '*collab-plugins*/src*' -name '*.rs' | xargs grep -ln 'checkpoint\|Checkpoint\|flush\|RocksdbBackup' 2>/dev/null
find ~/.cargo -path '*rocksdb*/src*' -name 'checkpoint*.rs' 2>/dev/null
```

Decision gate:
- If a checkpoint/flush path exists → continue to Step 2.
- If it does **not** → STOP. Record findings and escalate: the design must switch to "flush + copy under an exclusive txn" or "quiesce + plain dir copy". Do not fake a passing test.

- [ ] **Step 2: Write the failing spike test**

`frontend/rust-lib/flowy-backup/tests/checkpoint_spike.rs`:

```rust
// Opens a CollabKVDB, writes a document, spawns a thread writing continuously,
// takes a checkpoint into out_dir, then opens the checkpoint as a fresh
// CollabKVDB and asserts the first document reads back identically.
//
// The exact CollabKVDB open/write/read calls are filled from Step 1 discovery;
// use collab-integrate's CollabPersistenceImpl / with_write_txn / read_txn as
// seen in flowy-user/src/migrations/*.rs for the real call shapes.
#[test]
fn checkpoint_of_live_collab_db_restores_identically() {
  // 1. open source CollabKVDB at src_dir
  // 2. write doc "obj-1" with known bytes
  // 3. spawn writer thread hammering "obj-churn" until a stop flag
  // 4. checkpoint_collab_db(src_dir, out_dir)
  // 5. stop writer
  // 6. open CollabKVDB at out_dir, read "obj-1", assert bytes equal
  todo!("fill from Step 1 discovery")
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p flowy-backup --test checkpoint_spike`
Expected: FAIL / panic on `todo!` — proves the harness is wired.

- [ ] **Step 4: Implement `checkpoint_collab_db` using the discovered API**

Fill `checkpoint_collab_db` in `snapshot.rs` with the real call found in Step 1 (RocksDB `Checkpoint::create`, or the collab-plugins backup API). Fill the test body with real `CollabKVDB` calls.

- [ ] **Step 5: Run the spike to verify it passes**

Run: `cargo test -p flowy-backup --test checkpoint_spike -- --nocapture`
Expected: PASS — checkpoint taken while a writer runs restores `obj-1` identically.

- [ ] **Step 6: GATE + commit**

If PASS, commit with the discovery findings in the message:

```bash
git add frontend/rust-lib/flowy-backup/
git commit -m "spike(backup): prove live collab_db RocksDB checkpoint restores identically

Findings: <checkpoint API used>, <live-handle vs fresh-handle>, <memtable/WAL flush needs>."
```

If FAIL and unfixable within 3 attempts → STOP, escalate to redesign per the spec's residual-risk path. Do not continue.

---

### Task 3: SQLite `VACUUM INTO` + write-barrier snapshot

**Files:**
- Modify: `frontend/rust-lib/flowy-backup/src/snapshot.rs`
- Test: `frontend/rust-lib/flowy-backup/tests/snapshot_roundtrip.rs`

**Interfaces:**
- Consumes: `checkpoint_collab_db` (Task 2), a SQLite connection source.
- Produces: `fn snapshot_sqlite(db_path: &Path, out_path: &Path) -> Result<(), BackupError>` and the full `SnapshotService::snapshot(...)` body that (a) takes the write barrier, (b) VACUUM INTOs SQLite, (c) checkpoints collab_db, (d) computes sha256 checksums, (e) returns `SnapshotManifest`.

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

- [ ] **Step 5: Implement `SnapshotService::snapshot` (barrier + both DBs + checksums)**

Assemble VACUUM INTO + `checkpoint_collab_db` into a staging dir, sha256 each output, return `SnapshotManifest`. The write barrier is passed in as a closure/guard by the caller (BackupManager, Plan 4) so this crate stays free of app-global locks; document the invariant that `snapshot` must be called inside the barrier.

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

- **Spec coverage (this plan's slice):** WAL prereq (Task 0) ✓; snapshot core / VACUUM INTO + checkpoint under write barrier (Tasks 2–3) ✓; checksum verification (Tasks 3–4) ✓; local restore round-trip (Task 4) ✓. Deferred to later plans (explicitly out of Plan 1 scope): Drive client/OAuth/manifest-last, staged restore-on-launch boot path, 2a guardrail persistence, retention/backoff, Flutter UI. These are named in the spec's Implementation Sequencing §2–6.
- **Placeholder scan:** the only intentional `todo!`/discovery points are in the Task 2 spike, which is by nature an investigation; every other step carries real code or a real command. Task 0 Step 1 flags that constructor names must be confirmed against `pool.rs`.
- **Type consistency:** `SnapshotService`, `SnapshotManifest`, `BackupError`, `checkpoint_collab_db`, `snapshot_sqlite`, `snapshot`, `restore` names are used consistently across Tasks 1–4.

## Gate

Task 2 (spike) is a hard gate. If it fails and cannot be resolved in 3 attempts, stop and revisit the spec's approach before writing Plans 2–5.
