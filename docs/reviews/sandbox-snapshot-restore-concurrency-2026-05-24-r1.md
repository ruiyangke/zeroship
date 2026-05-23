# Concurrency Review — sandbox snapshot/restore

**Branch:** `feat/sandbox-snapshot-restore` (HEAD `b048b491`)
**Scope:** `crates/sandbox/**`
**Date:** 2026-05-24 UTC · **Reviewer:** code-critic (Opus)

---

## CRITICAL

### 1. `update_lessee` has zero callers — lease-takeover is dead code
`db.rs:2305` defines `update_lessee` ("Called every 10s by the in-flight snapshot/restore handlers"). **No caller exists.** With `update_sandbox_status` never touching `lessee_updated_at` (`db.rs:1712-1724`) and `update_snapshot_metadata` setting it NULL (`db.rs:2164`), every transient row keeps `lessee_updated_at = NULL`. The sweep filters `WHERE lessee_updated_at IS NOT NULL` (`db.rs:2361`), so transient-state takeover (§6.1) **never fires**.

### 2. Blocking `ureq` / `std::process::Command` on ntex async worker
`RealRestoreBackend::submit_restore_job` (`restore_handler.rs:789`) and `wait_for_livez` (line 815) call `nomad_post_blocking` + `wait_for_alloc_running_blocking` + `wait_for_livez_blocking` — ureq-blocking with `std::thread::sleep` loops (lines 1088, 1121). Invoked synchronously from `do_restore_inner` (lines 384, 389), `await`-ed from the ntex handler `admin_handlers::wake_sandbox` (`admin_handlers.rs:1259`). Budget: up to 60 s alloc + 30 s livez = **90 s ntex stall per wake**. Same pattern: `teardown_restore` (line 823), `store.get(...)` for `GcsSnapshotStore` (line 323), `ch.pause/snapshot` via `std::process::Command` (`snapshot_handler.rs:335-338`, 30 s `CH_REMOTE_TIMEOUT`). Contrast `persist.rs:641,656,676` which wraps every `std::fs` call in `spawn_blocking`.

### 3. Restore success leaks `vm_index` reservation
`do_restore_inner` reserves at `restore_handler.rs:298`; the success branch (line 411) never calls `release_vm_index`. Only `teardown_restore` releases (line 832). After any successful wake, the slot in `RealRestoreBackend::reservations` (line 713) stays held forever. A second snapshot→wake cycle in the same process returns `VmIndexUnavailable` (line 299), and rollback fires a spurious `nomad_delete_blocking` for a never-submitted job (lines 818-833).

---

## IMPORTANT

### 4. `GcsSnapshotStore::access_token` holds Mutex across 5 s blocking HTTP
`snapshot_store_gcs.rs:148` locks `cached_token` and holds it across `ureq::get(METADATA_TOKEN_URL).call()` (line 159). N concurrent restores serialize ≤5 s on every token refresh.

### 5. Inconsistent `RwLock` poisoning policy
`registry.rs` lines 239,298,308,319,331,347,363,385,428,474,483,496,511,527,552,559 use bare `.unwrap()`; `registry.rs:190,255`, `restore_handler.rs:527,535,748,755`, `sweep.rs:254`, `snapshot_store_gcs.rs:151` use `unwrap_or_else(|p| p.into_inner())`. A panic inside any registry writer cascade-poisons every reader on `by_sandbox`, killing the controller.

### 6. Restore rollback writes stale `g1` — orphan VM on generation race
`restore_handler.rs:210`: `update_sandbox_status(target, g1, None)`. If anything bumps generation between line 190 (`g1`) and rollback (e.g. peer-host `lease_take_over` at `db.rs:1531`, or a future `update_lessee` fix), the CAS misses but `backend.teardown_restore` (line 208) **still** kills the alloc and releases the vm_index. Net state: alloc gone, pg row stuck in `Restoring`. The log says *"may be wedged until lease-takeover sweep"* — but per finding #1 that sweep doesn't fire.

### 7. Snapshot rollback after CH pause leaves an orphan `running` row
`snapshot_handler.rs:286-313`: `do_snapshot_inner` calls `ch.pause()` + `ch.snapshot()` (lines 335-338) which crash the VMM post-snapshot (v51.1 quirk, line 467). If GCS upload then fails (line 345), rollback writes status=`running` — but the source VM is dead. Gateway 5xx until orphan-prune sweep.

### 8. `std::fs` on async path (`restore_handler.rs:303,309,335,443,474`)
`do_restore_inner` runs `remove_dir_all`, `create_dir_all`, `metadata` ×3, `rewrite_config_json` (read+write) without `spawn_blocking`. Same crate's `persist.rs:641,656,676` shows the correct pattern.

---

## MINOR

### 9. Sweep CAS fences on local host_id — no cross-host transient recovery
`sweep.rs:161` calls `db.update_sandbox_status(...)` which fences on `host_id = self.host_id()` (`db.rs:1720`). A row owned by dead host A can only be recovered by A's own sweep. `lease_take_over` (`db.rs:1531`) does cross-host CAS but filters `status IN ('starting','running','unreachable')` (line 1538), excluding transients. With finding #1, cross-host transient recovery has no working path.

### 10. Stub backends use `std::sync::Mutex` reachable from async (test-only)
`sweep.rs:248` `RecordingIdleSnapshotter::seen: std::sync::Mutex<Vec<Uuid>>`; held across `snapshot_one` from async `run_idle_eviction_once`. Mirrors `restore_handler.rs:527,535` `StubRestoreBackend`. Fixture-only impact.

### 11. Pattern asymmetry: `persist.rs` does it right, snapshot/restore don't
`persist.rs:641,656,676` wraps every `std::fs` in `spawn_blocking`. The new handlers ignore the in-house contract.

---

**Summary:** 3 CRITICAL, 5 IMPORTANT, 3 MINOR. Top two: **(1)** lease-takeover sweep is dead code — `update_lessee` has no callers and `update_sandbox_status` never sets `lessee_updated_at`, so §6.1 never fires (`db.rs:2305` + `db.rs:1712-1724` + `db.rs:2361`); **(2)** restore/snapshot do ≤90 s of blocking ureq + `Command` + `std::fs` on the ntex worker (`restore_handler.rs:789,815,823` + `snapshot_handler.rs:335-338`).
