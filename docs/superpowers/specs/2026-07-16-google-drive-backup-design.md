# Google Drive Backup & Sequential Sync — Design

**Date:** 2026-07-16
**Status:** Approved design, pending implementation plan
**Scope:** Desktop-only (macOS / Windows / Linux), v1

## Problem & Goals

Let AppFlowy users host their own data in their own Google Drive, for two purposes:

1. **Backup / data ownership** — a complete, restorable copy of the user's AppFlowy data lives in their Drive, independent of AppFlowy Cloud.
2. **Sequential single-device sync (2a)** — the user works on one device at a time and moves their state between machines via Drive: back up on device A, restore on device B before editing.

Explicitly **not** in scope: concurrent multi-device editing with automatic merge (2b). AppFlowy's documents are CRDTs whose only real-time merge path is AppFlowy Cloud's WebSocket collab protocol. Google Drive is a file store and cannot merge document state; attempting concurrent sync through it causes conflicted-copy data loss. This design deliberately targets 2a only.

## Architectural Constraints (why this shape)

Established by codebase exploration:

- User content lives as CRDTs in `{app_data_dir}/{uid}/collab_db/` (a **RocksDB** KV store, `CollabKVDB`) plus relational metadata in `{app_data_dir}/{uid}/flowy-database.db` (**SQLite**, via Diesel).
- There is **no document-level storage abstraction** Drive could plug into. The only S3-like trait (`StorageCloudService`, `flowy-storage-pub/src/cloud.rs`) covers **file attachments only** and is disabled in Local mode.
- Therefore "swap the backend to Drive" is impossible. The only viable mechanism is snapshotting the **data directory itself** in and out of Drive.
- Consistent-copy primitive per store: SQLite `VACUUM INTO` (safe on a live WAL DB); `collab_db` (RocksDB) has **no reachable live-snapshot primitive** through AppFlowy's `collab-plugins` fork, so it uses **Drop-and-Copy** (close the handle → copy quiescent dir → reopen). See the collab_db mechanism section for the full rationale. (The `RocksdbBackup` hook in `collab-integrate/src/collab_builder.rs` is dead, doc-level-only code — not a full-DB snapshot.)

**Fork constraints (hard — this repo is a merge-friendly fork of upstream AppFlowy):**
- Keep custom work in **new, separate crates/modules** (`flowy-backup`); avoid editing upstream files where an additive hook works instead.
- **Do not rename** upstream symbols/files.
- **Do not fork external dependencies** (`AppFlowy-Collab`/`collab-plugins`, `rust-rocksdb`) — this is why the native-checkpoint-via-FFI approach was rejected in favor of Drop-and-Copy, which needs no dependency changes.

## Non-Goals (v1)

- Mobile (iOS / Android).
- Idle / automatic / on-close snapshots (manual button only for v1; live-snapshot machinery is built so these can be layered on later without rework).
- Incremental / delta snapshots (full snapshot each time).
- Concurrent multi-device editing (2b).
- Google Drive as an attachment (`StorageCloudService`) backend.
- Client-side encryption of snapshots (plaintext for v1; encryption is a noted fast-follow).

## Architecture

New Rust crate **`flowy-backup`** under `frontend/rust-lib/`, composed of three units with clear boundaries:

| Unit | Responsibility | Depends on |
|------|----------------|------------|
| `SnapshotService` | Build a consistent on-disk snapshot of the data dir; consume a snapshot on restore | collab_db drop-and-copy, SQLite `VACUUM INTO`, zip |
| `DriveClient` | Google Drive REST v3: OAuth, resumable upload, download, list, delete | HTTP client, OS keychain |
| `BackupManager` | Orchestrate backup/restore flows; own manifest + version logic + guardrail | `SnapshotService`, `DriveClient` |

The Flutter layer provides UI only: a **"Back up to Drive"** button, a **"Restore from Drive"** button, and a **"Connect Google Drive"** flow in Settings. All snapshot and network work happens in Rust, invoked across the existing event/FFI boundary. Flutter holds no snapshot or Drive logic.

## Data Model

**Backup unit:** the whole per-user data dir `{app_data_dir}/{uid}/`. All workspaces for the account travel together.

**Included in a snapshot:**
- `flowy-database.db` — copied via `VACUUM INTO` (consistent; SQLite has no background mutators, so a live consistent copy is safe once WAL is genuinely on).
- `collab_db/` — copied via **Drop-and-Copy** (see below). The originally-assumed live `Checkpoint::create` is **not reachable** and has been removed from the design.

**Excluded:**
- `indexes/` — Tantivy full-text index, rebuilds on next app open.
- `cache_files/` — transient upload staging.

**collab_db mechanism — Drop-and-Copy (supersedes the spec's original live-checkpoint assumption).** Investigation (Plan-1 spike, 2026-07-16) established that AppFlowy's `collab-plugins` fork does **not** expose a usable live snapshot of `collab_db`:
- `CollabKVDB` hides its inner `rocksdb::TransactionDB` (private, no getter/Deref), so RocksDB's native `Checkpoint`/`BackupEngine` cannot be reached, and `rust-rocksdb`'s `Checkpoint` requires a trait `TransactionDB` does not implement.
- `CollabKVDB::flush()` is a **no-op** (`kv_impl.rs`), so "flush then copy" flushes nothing.
- Byte-copying the directory while the handle is open is **unsafe** — RocksDB background compaction/flush threads mutate SST files independently of user write transactions, producing a torn snapshot. (Note: AppFlowy's *existing* `db.rs` zip-backup does exactly this open-dir copy and is therefore unsafe for recovery.)

The only mechanism that is both correct and does not require forking an external dependency (a hard constraint — see "Fork constraints" below) is to make the store **quiescent** before copying: drop **all** `Arc<CollabKVDB>` references for the user so the handle fully closes (RocksDB syncs its WAL and stops all background threads on clean close), byte-copy the now-static directory, then reopen. Plan-1 spike (commit `3de588e`) confirmed this against a bare handle: `CollabKVDB` holds a single `Arc<TransactionDB>` and `open()` spawns no threads, so `drop` closes synchronously; a copy of the closed dir reopened with every committed entry intact, including one written immediately before the drop. **Production caveat (Plan 2):** the app shares the `CollabKVDB` `Arc` across ~10 holders and attaches per-object flush plugins via the collab builder — the app-wide quiesce must guarantee *every* strong reference (including any plugin/background thread) is released so the handle actually reaches refcount 0 before the copy; a lingering holder means the dir is not quiescent. A reopened copy replays the WAL, so all committed data is present. This blocks collaborative editing for the duration (close + copy + reopen — seconds, scaling with `collab_db` size), which is acceptable for a **manual** backup button.

**Quiesce coordination = the cross-DB consistency barrier (review blocker 3).** The same quiesce window that closes `collab_db` also pauses SQLite writers and runs `VACUUM INTO`, so both stores are captured with no writes in flight between them — no `collab_db` document whose `flowy-database.db` metadata is missing. The barrier — not copy ordering — guarantees consistency.

**Prerequisite — WAL must actually be enabled (review blocker 1):** `flowy-sqlite/src/sqlite_impl/pool.rs` currently sets the journal-mode pragma only `if journal_mode != WAL`; since WAL is the default value the pragma never runs and the DB is effectively in DELETE mode. Fixed in Plan-1 Task 0 (pragma now unconditional).

**Snapshot artifact:** a temp dir assembled from the above, zipped to `appflowy-snapshot-{uid}-v{N}-{deviceId}.zip`. Snapshots are atomic — assembled fully in a temp location, then finalized; a crash mid-build leaves no half-written artifact.

**Manifest (`manifest.json`, stored in Drive alongside snapshots):**
```json
{
  "version": 7,
  "deviceId": "<stable per-install uuid>",
  "uid": "<user id>",
  "timestamp": "2026-07-16T10:30:00Z",
  "appVersion": "<semver>",
  "snapshotFile": "appflowy-snapshot-...-v7-....zip",
  "checksums": { "flowy-database.db": "<sha256>", "collab_db": "<sha256>" }
}
```
`version` is a monotonic integer — the source of truth for the 2a guardrail. The manifest is small and fetched cheaply without downloading any zip.

## Drive Integration

- **Auth:** OAuth 2.0 desktop loopback + PKCE, scope **`drive.file`** (least privilege — the app can only access files it created, never the user's whole Drive). This is separate from any existing Google *login* OAuth. The refresh token is stored in the OS keychain.
- **Location:** a visible **"AppFlowy Backups"** folder in the user's My Drive, so the user can browse, download, and delete snapshots themselves (supports data ownership).
- **File-ID pinning + accessibility check (review high 7):** `drive.file` scope loses access to any file the *user* moves out of the folder or that the app did not create. The app therefore pins the Drive **file IDs** of the folder and the manifest (persisted in SQLite), addresses them by ID rather than by path/name, and runs an accessibility probe at the start of every backup/restore. If the folder or manifest is missing/inaccessible (user moved or deleted it), the app surfaces a clear "backup folder was moved or removed — reconnect / recreate" prompt instead of silently failing or forking a second folder.
- **Retention:** keep the last **5** snapshots; prune the oldest on successful backup. A snapshot zip with no referencing manifest entry is treated as garbage and eligible for cleanup.
- **Encryption:** none in v1 — snapshots are plaintext zips in the user's own Drive. Client-side encryption is a documented fast-follow.
- **OAuth lifetime gotchas (review high/medium 8-9):** while the Google Cloud OAuth app is in "testing" status, refresh tokens expire after 7 days — the app must be moved to "published"/verified for the `drive.file` scope before release, and the client handles refresh-token invalidation by re-prompting the connect flow.

## Flows

### Backup
1. User clicks **Back up to Drive**.
2. Run the 2a guardrail check (see below); if Drive is newer, prompt.
3. Verify Drive folder/manifest accessibility (by pinned file ID).
4. `SnapshotService` builds a consistent snapshot in one quiesce window (pause writers + `VACUUM INTO` SQLite; close `collab_db` handle + copy quiescent dir + reopen) → zip.
5. `DriveClient` reads current Drive `manifest.json`, computes next `version = N+1`.
6. Resumable-upload the zip **first** and confirm it is fully committed.
7. **Manifest is written last, as the single source of truth (review blocker 4).** The version only advances when the manifest update succeeds; a zip that exists without a manifest pointing at it is ignored and GC'd. Crash between zip-upload and manifest-write leaves the previous consistent manifest intact — no orphaned version, no monotonicity break.
8. Prune snapshots beyond the retention limit.
9. Record `last_backed_up_version = N+1` in SQLite.

### Restore (staged, applied at next launch — review blocker 2)
Windows cannot rename a directory over the open DB/collab handles the running app holds, so the swap is not performed live. Instead it is **staged and applied at next launch, before any handle opens.**

1. User clicks **Restore from Drive**.
2. Verify folder/manifest accessibility; fetch `manifest.json`; show version/device/timestamp; require explicit confirmation.
3. **Auto local safety snapshot** — same machinery, stored under `{app_data_dir}/backups/` — so a mis-click can't lose current local state.
4. Download the latest snapshot zip; **verify checksums** against the manifest. Any mismatch or download error → abort, nothing touched.
5. Unzip to a staging dir under `{app_data_dir}/` and write a **`restore-pending`** marker (naming the staging dir, target uid, and expected checksums). Nothing in the live data dir is touched yet.
6. Prompt the user to restart. Restore is durable across an unexpected exit because it is driven by the marker, not by in-memory state.
7. **On next launch, before opening any DB/collab handle**, the boot path sees `restore-pending`, re-verifies checksums, atomically swaps the staged dir into place (platform-aware: rename on Unix; rename-old-aside → move-new-in → delete-old on Windows), clears the marker, then proceeds to open handles.
8. Record `last_restored_version` in SQLite (post-swap).
9. Any failure during the boot swap → restore the safety snapshot and clear the marker; the app boots on the prior state.
10. Safety snapshots and consumed staging dirs are cleaned up after a successful boot swap and on a retention/age policy, so repeated restores don't leak disk.

## 2a Guardrail (stale-write prevention)

The failure this prevents: finish on device A → back up (Drive is newest) → on device B forget to restore → edit stale data → back up → device B's stale-plus-edits overwrites device A's newer work in Drive.

Mechanism (**soft version-check warning**):
- Every snapshot's manifest carries a monotonic `version` + `deviceId`.
- `last_restored_version` is persisted **in the SQLite database schema (review high 6)**, not in ephemeral cache/app state, so it survives cache wipes. A fresh install / cleared state reads as version 0, which — because 0 is below any real Drive version — forces the warning rather than silently allowing an overwrite (fail-safe direction).
- `deviceId` is a stable per-install random UUID persisted in SQLite, **not** derived from MAC/hostname (avoids collisions).
- On app open and before every backup, compare local against Drive's manifest. If `drive.version > last_restored_version` (Drive is newer than what this device last synced from), warn:
  > "Drive has a newer backup from {device} — restore first, or overwrite anyway?"
- The user can deliberately override. No hard lock (avoids the stuck-lock failure mode of a crashed device holding a checkout).
- **Residual risk (accepted for v1):** a soft guardrail can still be overridden into data loss by a user who dismisses the warning. This is an accepted trade-off of 2a-with-override; a hard lock is the documented alternative if it proves insufficient.

## Error Handling

- Resumable uploads survive network drops. The Drive manifest only advances after a fully successful upload, so Drive is never left pointing at a partial zip.
- **Rate limits, quota, session expiry (review medium 9):** the `DriveClient` implements exponential backoff with jitter on `403 rateLimitExceeded` / `429` and `5xx`; distinguishes transient (retry) from permanent (quota full, auth revoked) failures with distinct user-facing messages; and detects resumable-session expiry (Drive sessions are not indefinitely valid), restarting the session rather than reporting a false success. A backup is only reported successful once the manifest is committed.
- Restore is transactional around the safety snapshot: checksum verified before any swap; roll back on post-swap failure.
- OAuth token expiry → silent refresh. Revoked/invalid auth → re-prompt the connect flow.
- Snapshot build failure (e.g. copy/reopen error) → surface to user, ensure `collab_db` is reopened, no partial artifact left, no manifest change.
- **Disk hygiene (review medium 10):** safety snapshots and staging dirs are cleaned up after successful restore and on an age/count policy, so repeated failed restores don't exhaust disk.

## Implementation Sequencing

Ordered so the riskiest assumption is proven before any UI, Drive, or product work is built on top of it.

0. **Prerequisite:** fix the WAL pragma (`flowy-sqlite/src/sqlite_impl/pool.rs:148`) to run unconditionally; verify `PRAGMA journal_mode` actually returns `wal` at runtime.
1. **Spike — prove the snapshot core (gates everything). RESOLVED 2026-07-16:** the live-checkpoint assumption was disproven; mechanism is now **Drop-and-Copy**. The remaining spike work is to prove drop-and-copy correctness under a *realistic* TDD harness: seed a non-trivial `collab_db` (enough data to trigger RocksDB background compaction), write data, drop all handles, copy the quiescent dir, reopen the copy, and assert data (including writes committed immediately before the drop, i.e. WAL-replay) is byte-identical. The test must NOT be the hollow "serialized writer / tiny DB" pattern that the first spike failed on.
2. `SnapshotService` (snapshot + restore-staging, quiesce coordination, checksums) — local only, no Drive.
3. Staged restore-on-launch boot path + `restore-pending` marker + safety snapshot + rollback.
4. `DriveClient` (OAuth/PKCE, resumable upload, download, list, delete, backoff) against a mock, then real Drive.
5. `BackupManager` (manifest-last ordering, version/guardrail, retention, accessibility check).
6. Flutter UI (connect flow + two buttons + guardrail prompt).

## Testing (TDD)

- **Unit:** snapshot produces an openable SQLite DB and openable RocksDB; manifest version monotonicity; retention prune logic; `DriveClient` against a mock HTTP server (upload/download/list/delete, resumable-upload resume, token refresh).
- **Integration:** backup → wipe local data dir → restore → assert restored data is byte-identical to the original; stale-version guardrail fires when Drive is ahead and stays quiet when it isn't.

## Open Questions / Future Work

- Client-side encryption of snapshots (passphrase-based) — fast-follow after v1.
- Idle / on-close automatic snapshots (debounced + dedup) — the live-snapshot machinery in v1 is designed to support this.
- Incremental/delta snapshots to reduce upload size and Drive churn.
- Mobile support (requires a different mechanism given sandboxing and background-execution limits).
