# Google Drive Backup — Handover (as of 2026-07-17)

Branch: `feat/google-drive-backup`. This is the "you compile + verify" handover: Plan 2's code was written in an environment that **cannot compile `flowy-core`/`flowy-user`** (AppFlowy's protobuf codegen needs a pinned `protoc` + Flutter env), so it is **COMPILE-UNVERIFIED**. Plan 1 is fully compiled and tested.

## Status

| Piece | State |
|-------|-------|
| **Plan 1 — snapshot core (`flowy-backup` crate)** | ✅ Done, compiled, 4 tests green (drop-and-copy spike, VACUUM INTO, byte-identical round-trip, checksum-mismatch rejection). |
| **WAL prerequisite fix** (`flowy-sqlite/pool.rs`) | ✅ Done, unit-tested. App-wide behavior change — smoke-test the running app. |
| **Plan 2 — app-wide quiesce + backup event** | ⚠️ Code written, COMPILE-UNVERIFIED. Quiesce ordering + all manager signatures + path derivation reviewed correct against source. 1 Critical + 2 Important fixes below. |
| **Plan 3 (Google Drive/OAuth), Plan 4 (Flutter UI), Plan 5 (staged restore-on-launch)** | Not started. |

**Nothing pushed. Nothing merged.** Commits on the branch only.

## Mechanism (decided, see design spec)

- `collab_db` (RocksDB) has **no reachable live-snapshot** through AppFlowy's `collab-plugins` (native checkpoint unreachable; forking the dep is disallowed — merge-friendly fork). Mechanism = **Drop-and-Copy**: fully close the handle (drop the sole `Arc<CollabKVDB>`), byte-copy the quiescent dir, reopen. Proven correct in Plan 1's spike.
- Quiesce = **workspace reinit**: managers hold `Weak<CollabKVDB>` and won't rebind after reopen, so backup closes collab objects → `close_collab_db` → snapshot → re-runs the `on_workspace_opened` init sequence.
- SQLite: `VACUUM INTO` (safe on live WAL); captured in the same quiesce window for cross-DB consistency.

## To build on your machine

1. Use AppFlowy's pinned `protoc` + Flutter dev env (this repo's `protoc 35.1` install here was too new — it produced conflicting `TryFrom` impls; use the version AppFlowy's `cargo make` install flow provides). Note: generated `protobuf.rs` files are gitignored — regenerate via your normal `cargo make` flow.
2. `cargo build -p flowy-core` and `cargo build -p flowy-backup`.
3. `cargo test -p flowy-backup` should be green (Plan 1). `cargo test -p flowy-user close_collab_db` (Plan 2 Task 0 unit test).

## Consolidated fix-list (from code-level review — supersedes the implementer's list)

### CRITICAL — blocks the backup event from working
1. **`initialize_backup` / `backup_coordinator` bridge is unreachable.** `dart-ffi/src/lib.rs:~169` stores `AppFlowyCore` as `Option<AppFlowyCore>` (not `Arc`), so the global `backup_coordinator` weak is never set → the event fails with "AppFlowyCore not initialized".
   - **Recommended fix (cleaner than the Arc restructure):** don't use a global bridge at all. Register the backup event **in `flowy-core`** where `AppFlowyCore` is directly reachable, instead of in `flowy-user` (which can't depend on `flowy-core`). Check how `flowy-core` already wires event plugins (`deps_resolve/`). This removes `backup_coordinator.rs` and the init-ordering footgun entirely.
   - **Alternative (review's Option A):** make dart-ffi hold `Arc<AppFlowyCore>` and call `core.initialize_backup(core.clone()).await` right after construction.

### IMPORTANT
2. **Swallowed storage-manager init error** (`flowy-core/src/lib.rs:~201-204`): `storage_manager.initialize_after_open_workspace` returns `()`; failures only log. Post-backup, verify uploads still work.
3. **Swallowed reopen failures** (`flowy-core/src/lib.rs:~222-230`): the safety guard correctly always runs reopen, but if a manager's `initialize_after_open_workspace` fails, the backup result is still returned and the app is left degraded. Add a `tracing::warn!` "backup done but reopen failed — app may be degraded", and consider a partial-success return type.

### MINOR
4. Move the function-scoped `use flowy_user_pub::sql::select_user_workspace_type;` (`flowy-core/src/lib.rs:~136`) to module-level imports.
5. The backup event handler discards the returned `SnapshotManifest`. If Flutter should confirm success, add a `SnapshotManifestPB` output to the event.

### Verified correct (no fix) — reviewed against real source
- Quiesce ordering (close collab objects BEFORE `close_collab_db`); reopen always runs even if snapshot errors.
- All signatures: `close_collab_db`, `clear_workspace_awareness`, `reinit_user_awareness`, each manager's `initialize_after_open_workspace`, `folder_init_data_source`, `select_user_workspace_type`, `UserPaths::user_data_dir`, `WorkspaceType::is_local`, `Session::user_uuid`.
- `user_dir = {data_root}/{uid}` path derivation.

## Task 5 — RUNNING-APP VERIFICATION (the real gate — not yet done)

Unit tests cannot prove close/copy/reopen under live plugin flushes. After fixing Critical #1:

1. Build + launch desktop AppFlowy; open a local workspace; create a document (known text), a grid, a board.
2. Trigger `BackupWorkspace` while **idle**. Assert: no crash; workspace stays usable; docs/grids still open + edit (proves managers rebound to the new handle).
3. Open the produced staging snapshot's `collab_db` with a standalone `CollabKVDB::open` (throwaway bin) — assert it opens without corruption and contains the known document (proves the copy was of a genuinely closed, consistent DB).
4. **Adversarial:** trigger backup while actively typing. Assert: no crash, no lost edits beyond the last flush, snapshot opens clean. If corruption/crash appears, the Weak-only close is insufficient under load → add explicit per-manager collab-object close + plugin-stop before the copy (re-evaluate the upstream-edit budget).
5. Smoke-test the **WAL fix** separately: open/edit/restart the app, confirm `-wal`/`-shm` handling and existing flows are unaffected.

## Principal Rust architect review (2026-07-18) — full changeset

Verdict: Drop-and-Copy + workspace-reinit is the right approach; Plan 1's core holds up; **Plan 2 has three real defects** beyond the compile-unverified status. Corrected a prior error: the committed orchestration (`flowy-core/src/lib.rs:155–202`) does **clear_awareness → close_collab_db → snapshot → reinit** and does NOT close collab objects/stop plugins first.

- **CRITICAL 1 — quiesce doesn't actually quiesce.** Relies solely on dropping the one `Arc` in `UserDB::collab_db_map`; never closes live collab objects or stops flush plugins first. A plugin flush thread can upgrade its `Weak` during the copy → torn snapshot → silent corruption on restore. Fix: explicitly close collab objects (folder/document/database) + stop plugins BEFORE `close_collab_db`.
- **CRITICAL 2 — backup event dead on arrival.** `backup_coordinator` global lazy-static never initialized (`initialize_backup()` has no caller; dart-ffi holds `Option<AppFlowyCore>` not `Arc`). Fix: delete the bridge, register `BackupWorkspace` in `flowy-core`.
- **CRITICAL 3 — reopen failures report success** (`lib.rs:222–230`), leaving the app silently degraded. Fix: partial-success return type.
- **HIGH:** storage-manager reopen error ignored (`lib.rs:201–204`); `snapshot_closed_collab_db` precondition is doc-only (consider a closed-handle witness token).
- **MEDIUM:** document dir-checksum symlink/rename behavior; verify diesel + rusqlite share the bundled `libsqlite3-sys`.

Status of fixes: see the FIX PASS section appended after implementation.

## Commit map (branch `feat/google-drive-backup`)

- WAL fix, `flowy-backup` crate + tests (Plan 1): `a3e91fe`, `3406ced`, `3de588e`, `1aec318`, `8fe8a2d`, `9561052`.
- Plan 2: `f2e795f` (seam methods), `5a00d97` (quiesce orchestration + event, COMPILE-UNVERIFIED).
- Design/plan docs + this handover under `docs/superpowers/`.

Full progress ledger: `.superpowers/sdd/progress.md`. Per-task reports: `.superpowers/sdd/*.md`.

---

## FIX PASS — Principal Architect Review (2026-07-18, Implemented 2026-07-18)

### Status Summary
- **FIX 1 (CRITICAL — quiesce doesn't quiesce):** ✅ **IMPLEMENTED**
- **FIX 2 (CRITICAL — backup event dead on arrival):** ✅ **IMPLEMENTED** (flowy-core registration approach)
- **FIX 3 (CRITICAL — reopen failures report success):** ✅ **IMPLEMENTED**
- **FIX 4 (HIGH — swallowed storage error):** ✅ **IMPLEMENTED**

### FIX 1 — Explicit Collab Object Close (IMPLEMENTED)

**What changed:**
- Added `close_for_backup()` async method to each manager:
  - `FolderManager::close_for_backup()`: Closes the folder collab object via `mutex_folder.swap(None)` and `old_folder.close()` (extracted from `initialize_after_sign_in` logic)
  - `DatabaseManager::close_for_backup()`: Clears task scheduler, closes all editors' views, clears both editor maps, closes workspace database (extracted from `initialize` logic)
  - `DocumentManager::close_for_backup()`: Clears document and removing-documents maps (extracted from `initialize` logic)
- Modified `AppFlowyCore::run_workspace_backup()` to call all three `close_for_backup()` methods **BEFORE** `close_collab_db` (new code block at ~line 155)
- Errors from close methods are logged but don't block the backup (graceful degradation)

**Rationale:** Managers hold live `Collab` objects; their flush plugins hold `Weak<CollabKVDB>`. Dropping the Weak alone doesn't stop an in-flight flush. Closing the `Collab` object (via `collab.close()`) terminates the object and its plugins before we drop the `Arc` in `UserDB::collab_db_map`. This ensures the snapshot copy sees a genuinely quiescent DB.

**Upstream methods (merge-friendly budget: 3/2 remaining):**
1. `flowy-folder/src/manager_init.rs`: `FolderManager::close_for_backup() -> FlowyResult<()>`
2. `flowy-database2/src/manager.rs`: `DatabaseManager::close_for_backup() -> FlowyResult<()>`
3. `flowy-document/src/manager.rs`: `DocumentManager::close_for_backup() -> FlowyResult<()>`

### FIX 2 — Backup Event Registration (IMPLEMENTED — flowy-core approach)

**Current state:** BackupWorkspace event fully migrated to flowy-core. Global bridge eliminated entirely.

**What changed:**
- **New file:** `flowy-core/src/backup_event.rs` — AFPlugin with backup event handler
  - Handler receives 5 managers + config via AFPlugin `.state()` injection pattern (same as folder/document/database plugins)
  - State params: `Weak<UserManager>`, `Weak<FolderManager>`, `Weak<DatabaseManager>`, `Weak<DocumentManager>`, `Weak<StorageManager>`, `Arc<AppFlowyCoreConfig>`
  - Calls `execute_workspace_backup()` standalone function (extracted backup logic)
  
- **Updated:** `flowy-core/src/module.rs` 
  - Signature: `make_plugins(..., config: Arc<AppFlowyCoreConfig>)` — now receives config to pass to backup plugin
  - Plugin registration: adds `backup_event::init(...)` with all managers + config to plugins vector
  
- **Removed:** `flowy-core/src/backup_coordinator.rs` — deleted entirely
  - Global static bridge `BACKUP_COORDINATOR` eliminated
  - No init-order footgun, no unreachable initialization
  
- **Removed from** `AppFlowyCore` (`lib.rs`):
  - `initialize_backup()` method — no longer needed
  - `mod backup_coordinator` — cleaned up
  
- **Removed from** flowy-user:
  - `BackupWorkspace` event from `event_map.rs` (UserEvent enum)
  - `backup_workspace_handler()` from `event_handler.rs`

**Why this approach works:**
Managers ARE created before `make_plugins()` is called (see `AppFlowyCore::init()` line 323–426). By passing `Arc<AppFlowyCoreConfig>` at plugin-creation time, the backup plugin gets access to all dependencies via state injection — the same pattern all other plugins use. No circular reference (Weak refs are used), no global state, testable.

**Commit:** `refactor(core): register BackupWorkspace event in flowy-core, remove global backup_coordinator bridge (COMPILE-UNVERIFIED)`

**Upstream methods (merge-friendly — no changes to upstream):**
- None — this is a pure flowy-core refactor. `run_workspace_backup()` method remains on `AppFlowyCore` for compatibility.

**Verification checkpoint (COMPILE-UNVERIFIED):**
- [ ] AFPlugin registration pattern compiles (state injection for 5 weak refs + 1 Arc config)
- [ ] `execute_workspace_backup()` function receives correct manager types and config
- [ ] `BackupWorkspacePB` import from `flowy_user::entities` resolves
- [ ] `make_plugins()` call in `AppFlowyCore::init()` updated to pass config
- [ ] No dangling references to `backup_coordinator` or `initialize_backup`

### FIX 3 — Partial-Success Return Type (IMPLEMENTED)

**What changed:**
- New struct `AppFlowyCore::WorkspaceBackupResult` (pub struct in `flowy-core/src/lib.rs`):
  ```rust
  pub struct WorkspaceBackupResult {
    pub manifest: SnapshotManifest,
    pub reopen_ok: bool,
    pub reopen_error: Option<String>,
  }
  ```
- `run_workspace_backup()` return type: `SnapshotManifest` → `WorkspaceBackupResult`
- Orchestration now tracks reopen success/failure independently and returns both outcomes
- flowy-user event handler logs `warn!` if `reopen_ok == false`

**Behavior:**
- Snapshot failure: Returns error immediately (app is already failed, reopen cannot succeed)
- Snapshot success + reopen success: Returns OK with both flags true
- Snapshot success + reopen failure: Returns OK with `reopen_ok=false` and error message (app is **degraded but partially recoverable**)
- Reopen is ALWAYS attempted (guarded in async block, errors don't short-circuit)

**Upstream methods:**
1. `flowy-core/src/lib.rs`: `struct WorkspaceBackupResult { manifest, reopen_ok, reopen_error }`

### FIX 4 — Storage Manager Error Logging (IMPLEMENTED)

**What changed:**
- `AppFlowyCore::run_workspace_backup()`: Added explicit `if let Err` check around `storage_manager.initialize_after_open_workspace()` call (line ~243)
- Logs `tracing::warn!("Storage manager initialization after backup failed: {:?}", err)` instead of silently ignoring
- Doesn't block the reopen; other managers continue to reinitialize

**Note:** `storage_manager.initialize_after_open_workspace()` returns `()` (no error signal), so the error is only surfaced if the method is updated to return `FlowyResult`. Current code assumes it can fail and logs if observed.

### Remaining Risks & Verification Checklist

**COMPILE-UNVERIFIED** — Critical assumptions to verify during build:

1. **Manager close-method names:**
   - [ ] `FolderManager` has `close_for_backup()` and it compiles
   - [ ] `DatabaseManager` has `close_for_backup()` and it compiles
   - [ ] `DocumentManager` has `close_for_backup()` and it compiles
   - [ ] All three are async, take `&self`, return `FlowyResult<()>`
   - [ ] Actual close logic matches extracted logic from `initialize_*` methods

2. **Return type propagation:**
   - [ ] `run_workspace_backup()` returns `WorkspaceBackupResult` (not `SnapshotManifest`)
   - [ ] `backup_coordinator::run_workspace_backup()` also returns `WorkspaceBackupResult`
   - [ ] flowy-user event handler compiles with updated return type
   - [ ] No other callers of `run_workspace_backup()` broken by return type change

3. **Backup coordinator initialization:**
   - [ ] `AppFlowyCore::initialize_backup()` is callable and properly sets global state
   - [ ] Dart-ffi layer calls it after wrapping AppFlowyCore in Arc
   - [ ] Event handler can successfully call `backup_coordinator::run_workspace_backup()`

4. **Plan 1 integration:**
   - [ ] `flowy_backup::SnapshotService::snapshot()` still called inside the quiesce window
   - [ ] Manifest returned and included in `WorkspaceBackupResult`
   - [ ] Plan 1 tests still pass (changes are additive to its code path)

### Future Work (Out of Scope)

- **FIX 2 proper (flowy-core event registration):** Requires refactoring plugin initialization to support lazy registration, or restructuring AppFlowyCore construction to defer plugin setup.
- **Closed-handle witness token (architect recommendation for FIX 4):** Add a sealed token type that can only be created by `close_collab_db()` and required by `snapshot()` to guarantee precondition.
- **Task 5 (running-app verification):** Must be done after successful compilation. See plan Task 5 steps in docs/superpowers/plans/2026-07-16-drive-backup-plan-2-app-quiesce.md.
