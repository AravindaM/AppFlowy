# Drive Backup — Plan 2: App-Wide Quiesce + Backup Wiring

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Let the running desktop app produce a consistent on-disk snapshot of the live workspace by momentarily quiescing `collab_db` (closing all collab objects → dropping the sole `Arc<CollabKVDB>` → copy → reopen → rebuild managers), then hand the copied dir to the Plan-1 `flowy-backup` `SnapshotService`.

**Architecture:** Reuse AppFlowy's existing workspace-reinit machinery (`*_manager.initialize_after_open_workspace()`), add two tiny upstream methods to close `collab_db` and clear user-awareness, and orchestrate the quiesce from `AppFlowyCore`. Expose it as one additive user event that Flutter can call. Correctness (no torn copy, no corruption, no lost edits, no crash) is verified against the RUNNING desktop app — not unit tests alone.

**Tech Stack:** Rust (`flowy-core`, `flowy-user`), the Plan-1 `flowy-backup` crate, AppFlowy event system (AFPlugin), Flutter (later plan for UI — this plan wires the event only).

## Global Constraints

- Desktop only.
- **Merge-friendly fork:** minimize upstream edits. Budget: **2 small new methods** in upstream files (`flowy-user/src/services/db.rs`, `flowy-user/src/user_manager/manager.rs`) + the orchestration method + one event registration. No renames, no signature changes to existing methods, no external dep changes.
- **collab_db close model (from investigation):** managers hold only `Weak<CollabKVDB>`; the single strong `Arc` is `UserDB::collab_db_map`. Dropping it (`close_collab_db`) closes RocksDB. The RocksdbDiskPlugin also holds a `Weak` and flushes on upgrade.
- **Correctness gate:** the copy must happen only when `collab_db` is genuinely closed (no live `Arc`, no in-flight plugin flush). This is unprovable by unit tests → **running-app verification is mandatory** (Task 5).
- Reuses Plan-1 `SnapshotService::snapshot(user_dir, staging_dir) -> SnapshotManifest` and `restore(...)` (already built + tested).

## Non-Goals (Plan 2)

- Google Drive, OAuth, upload/download, manifest-in-Drive (Plan 3).
- Flutter UI / backup button (Plan 4) — this plan wires the callable event only, tested via the event, not a button.
- Staged restore-on-launch boot swap (Plan 5).

## Key runtime facts (verified, with paths)

- `flowy-core/src/app_life_cycle.rs:~396` `on_workspace_opened` calls each manager's `initialize_after_open_workspace(...)`. `FolderManager` and `DatabaseManager` close their old collab objects inside that call before rebuilding; `DocumentManager` clears caches (documents are opened on demand).
- `on_workspace_closed` (`app_life_cycle.rs:~391`) currently only closes the search index — it does NOT close managers or `collab_db`.
- `flowy-user/src/services/db.rs` `UserDB { collab_db_map: DashMap<i64, Arc<CollabKVDB>> }`; `close(uid)` (sign-out) removes+drops the Arc. Managers get `Weak` via `get_collab_db()`.
- `flowy-user/src/user_manager/manager_user_awareness.rs` `user_awareness_by_workspace: DashMap<Uuid, Arc<RwLock<UserAwareness>>>` holds a `Weak<CollabKVDB>` and is never cleared on workspace switch.
- Orchestration owner: `AppFlowyCore` (`flowy-core/src/lib.rs`) owns `user_manager`, `folder_manager`, `document_manager`, `database_manager`.

---

### Task 0: Upstream seam — `close_collab_db` + `clear_workspace_awareness`

**Files:**
- Modify: `frontend/rust-lib/flowy-user/src/services/db.rs` (add `close_collab_db`)
- Modify: `frontend/rust-lib/flowy-user/src/user_manager/manager.rs` (add `clear_workspace_awareness`)
- Test: unit test in `db.rs` for `close_collab_db`

**Interfaces:**
- Produces: `UserDB::close_collab_db(&self, uid: i64) -> Result<(), FlowyError>` (remove Arc from map, drop, return); `UserManager::clear_workspace_awareness(&self, workspace_id: &Uuid)`.

- [ ] **Step 1: Failing test for `close_collab_db`**

In `db.rs` tests: open a `UserDB`, call `get_collab_db(uid)` to populate the map, assert present; call `close_collab_db(uid)`; assert the map no longer contains `uid` and a subsequent `get_collab_db(uid)` opens a fresh handle (Weak from before is dead). Use the crate's existing test harness for building a `UserDB` (follow existing tests in the file).

- [ ] **Step 2: Run, verify fail** — `cargo test -p flowy-user close_collab_db` → FAIL (method missing).

- [ ] **Step 3: Implement**

```rust
// db.rs — mirror existing close(), but keep sign-out's close() untouched.
pub fn close_collab_db(&self, user_id: i64) -> Result<(), FlowyError> {
  if let Some((_, db)) = self.collab_db_map.remove(&user_id) {
    let _ = db.flush();
    drop(db);
  }
  Ok(())
}
```
```rust
// manager.rs
pub fn clear_workspace_awareness(&self, workspace_id: &Uuid) {
  self.user_awareness_by_workspace.remove(workspace_id);
}
```

- [ ] **Step 4: Run, verify pass** — `cargo test -p flowy-user close_collab_db` → PASS.

- [ ] **Step 5: Commit** — `feat(user): add close_collab_db + clear_workspace_awareness seams for backup quiesce` (named files only).

---

### Task 1: Quiesce orchestration in `AppFlowyCore` (no copy yet — prove the close)

**Files:**
- Modify: `frontend/rust-lib/flowy-core/src/lib.rs` (add `quiesce_collab_db_for_backup` returning the closed `collab_db` path + a reopen/reinit closure or a two-method pair `begin`/`end`)

**Interfaces:**
- Produces: `AppFlowyCore::run_workspace_backup(&self, staging_dir: &Path) -> FlowyResult<SnapshotManifest>` that internally: (a) closes all collab objects, (b) clears awareness, (c) `close_collab_db`, (d) calls `SnapshotService::snapshot(user_dir, staging_dir)`, (e) reopens by re-running `initialize_after_open_workspace` for each manager + `initial_user_awareness`. For this task, stub (d) as a no-op/log and just prove (a)-(c)+(e) leave the app functional.

- [ ] **Step 1: Implement the close→reinit sequence (no copy)**

Sequence (call REAL methods; adapt arg lists to the actual signatures found in `app_life_cycle.rs` / `manager_user_workspace.rs`):
1. Resolve current `uid`, `workspace_id`, `workspace_type`, and the `data_source` used by `on_workspace_opened`.
2. Close collab objects: reuse the existing per-manager close path. **If a pure "close" method does not exist separately from `initialize_after_open_workspace` (which closes+reopens), that is the ordering hazard — see Task 1 Step 3.**
3. `user_manager.clear_workspace_awareness(&workspace_id)`.
4. `user_manager.authenticate_user.database.close_collab_db(uid)`.
5. [copy stub]
6. Reopen: `folder_manager.initialize_after_open_workspace(...)`, `database_manager.initialize_after_open_workspace(...)`, `document_manager.initialize_after_open_workspace(...)`, `user_manager.initial_user_awareness(...)`.

- [ ] **Step 2: Resolve the close/reopen ordering hazard (design decision, document it)**

`initialize_after_open_workspace` closes-then-reopens in one call. To reach a FULLY closed `collab_db` before the copy, we must ensure no collab object re-opens the handle between close and copy. Two viable shapes — pick one and document why in the code:
- **(a) Close-only via drop:** since managers hold `Weak`, simply `close_collab_db(uid)` after ensuring no active edit; the managers' existing objects keep dead Weaks, and we rebuild them after copy with `initialize_after_open_workspace`. Do NOT call initialize before the copy. (Simplest; relies on no live `Arc` upgrade during copy.)
- **(b) Explicit close methods:** if any manager holds a strong `Arc` (verify per manager — investigation says Weak-only, but CONFIRM `DatabaseManager`/user-awareness don't hold `Arc<CollabKVDB>` transiently), add a minimal `close_for_backup()` to that manager (counts against the upstream budget).
Confirm by asserting, in Task 5's app run, that `collab_db` reopens cleanly (proof it was closed).

- [ ] **Step 3: Build** — `cargo build -p flowy-core`. (This task has no unit test; its real verification is Task 5 against the running app — DO NOT fake a unit test that asserts nothing.)

- [ ] **Step 4: Commit** — `feat(core): workspace quiesce+reinit sequence for backup (no copy yet)`.

---

### Task 2: Wire the snapshot into the quiesce window

**Files:** Modify `frontend/rust-lib/flowy-core/src/lib.rs`; add `flowy-backup` as a dependency of `flowy-core` (workspace dep).

- [ ] **Step 1:** Replace the copy stub with `SnapshotService::new(data_root).snapshot(user_dir, staging_dir)?`, computing `data_root`/`user_dir` from the same path source `UserDB` uses. Return the `SnapshotManifest`.
- [ ] **Step 2:** Ensure reopen/reinit runs even if `snapshot` errors (wrap in a guard so a failed backup still restores a working app — reopen in a `finally`-style block). This is critical: a backup failure must NOT leave the app with a closed db.
- [ ] **Step 3: Build** — `cargo build -p flowy-core`.
- [ ] **Step 4: Commit** — `feat(core): take Plan-1 snapshot inside the quiesce window`.

---

### Task 3: Additive backup event (callable from Flutter)

**Files:** Modify `frontend/rust-lib/flowy-user/src/event_map.rs` (or a more fitting crate's event map) to add one event variant + handler that calls `AppFlowyCore::run_workspace_backup`.

- [ ] **Step 1:** Add `UserEvent::BackupWorkspace` (next free discriminant) + an async handler in `event_handler.rs` that resolves the staging dir and calls the core method, returning a small PB with the manifest summary (version/paths/checksums).
- [ ] **Step 2: Build** — `cargo build -p flowy-user && cargo build -p flowy-core`.
- [ ] **Step 3: Commit** — `feat(user): add BackupWorkspace event wiring`.

---

### Task 4: Edit-safety — flush before teardown

**Files:** within the orchestration (`flowy-core/src/lib.rs`).

- [ ] **Step 1:** Confirm (from `app_life_cycle.rs` teardown) whether collab objects flush-on-close. If the existing close path flushes, rely on it; if not, trigger the managers' existing save/flush before closing so the user's latest edits are in the snapshot. Document which.
- [ ] **Step 2: Build + commit** — `feat(core): ensure latest edits are flushed before backup quiesce`.

---

### Task 5: RUNNING-APP VERIFICATION (the real gate — not unit tests)

**This is mandatory and gates the plan. Unit tests cannot prove close/copy/reopen correctness under live plugin flushes.**

- [ ] **Step 1:** Build and launch the desktop app (see the project's run skill / `appflowy_flutter`). Sign in / open a local workspace, create a document with known content, a grid, and a board.
- [ ] **Step 2:** Trigger `BackupWorkspace` (via a temporary debug trigger or the event) WHILE the app is idle-but-open. Confirm: no crash, the workspace remains usable, documents/grids still open and edit correctly (proves reopen+reinit rebound the managers to the new handle).
- [ ] **Step 3:** Inspect the produced staging snapshot: open the copied `collab_db` with a standalone `CollabKVDB::open` (a throwaway test bin) and confirm it opens without "corruption"/"IO error" and contains the known document — proving the copy was of a genuinely closed, consistent DB.
- [ ] **Step 4 (adversarial):** Trigger the backup while actively editing a document (typing), then verify: no crash, no lost edits beyond the last flush, and the snapshot opens cleanly. If corruption or crash appears, the close is NOT reaching a quiescent state under load → escalate (add explicit collab-object close + plugin-stop before copy). Max 3 attempts, then reassess approach.
- [ ] **Step 5:** Record the verification evidence (commands, observations, screenshots/log excerpts) in `docs/superpowers/specs/` or the ledger. Commit any fixes from Step 4.

---

## Self-Review

- **Spec coverage:** app-wide quiesce (Tasks 0–1), snapshot in quiesce window (Task 2), callable entry point (Task 3), edit-safety (Task 4), running-app correctness gate (Task 5). Drive/UI/boot-swap remain Plans 3–5.
- **Placeholder scan:** Task 1/2 arg lists are intentionally "adapt to real signatures" because the exact `initialize_after_open_workspace` argument tuples must be read from the code at implementation time; every step names the real method + file. No `todo!` that asserts nothing ships — Task 1/2 verification is Task 5, explicitly.
- **Risk:** highest of any plan — touches core lifecycle. The `finally`-style reopen guard (Task 2 Step 2) and the running-app gate (Task 5) are the safety nets. If Task 5 Step 4 shows live-edit corruption, the Weak-only close model is insufficient and the plan must add explicit per-manager close + plugin-stop (larger upstream edit — re-evaluate merge-friendliness budget with the user).

## Gate

Task 5 (running-app verification) is the plan's gate. No claim of "backup works" is valid until Task 5 Steps 2–4 pass with observed evidence.
