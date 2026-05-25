# Sandbox/snapshot-restore — concurrency r10 review

Date: 2026-05-25 (UTC)
HEAD at audit: `e7b3278b`
Round 10 of N. Read-only. Branch `feat/sandbox-snapshot-restore`.

## Summary
- 5 findings (2 critical, 1 important, 2 minor).
- One pin-compat clearance: `LeasedVmSlot` RAII is NOT incompatible with compio's pinning rules — the prior implicit reason for deferral does not hold. RAII can land now.
- Carry-forwards from r1-r9 unchanged: **R4-A2 / C3 (6× widened) / C1-FOLLOWUP / R7-C1** all still open.

## Findings (NEW since r9)

### [R10-C1] Rollback path leaks state-map ghost after `register_restored` (CRITICAL, concurrency-r10)
- **Files**: `crates/sandbox/src/restore_handler.rs:261-289` (rollback), `:575-582` (register_restored call), `:1135-1150` (`RealRestoreBackend::teardown_restore`), `crates/sandbox/src/backend/nomad_ch.rs:1659-1690` (state-map insert).
- **Symptom**: If `do_restore_inner` returns Err **after** `register_restored` succeeds (line 575-582) — which is exactly what happens when the post-`register_restored` `update_sandbox_status(Running)` at `:600-602` returns `CasLost` (sweep won the row, or pg blip) — the outer rollback calls `backend.teardown_restore(sandbox_id, snap.vm_index)`. `teardown_restore` does TWO things and TWO things only: (a) nomad DELETE the restore job, (b) `release_vm_index`. **It does not remove the entry from the `NomadCHBackend::state` HashMap that `register_restored` just inserted.** Result: a ghost sandbox entry with a Nomad job that no longer exists, holding a now-released vm_index.
- **Race shape**:
  1. T₀: restore handler inserts `sandbox_id → NomadChSandbox{vm_index=N, user_id=U, …}` into state map (`:1671-1690`, success).
  2. T₁: `update_sandbox_status(Running)` await fails with `CasLost` (e.g., sweep ran in parallel and pushed row to `Snapshotted`).
  3. T₂: rollback closure calls `teardown_restore` → vm_index N goes back into `VmIndexAllocator::freed`.
  4. T₃: a brand new `create()` arrives; `vm_index_allocator.alloc()` hands out N to a different sandbox. State map now has TWO entries with vm_index=N (the ghost and the new tenant).
  5. T₄ (if the ghost's `user_id` is later reused): `NomadCHBackend::create_inner` lines `:533-550` enumerate "stop existing for user" — finds the ghost, calls `self.stop(ghost_id).await` → `stop_inner` removes ghost from state map AND calls `release_vm_index(N)`. N is **already in use by the new tenant**. Since `VmIndexAllocator::release` is `BTreeSet::insert` it is idempotent against double-release, but now N is in `freed` while live → next `alloc()` hands it out to a THIRD sandbox. Three tenants share vm_index N (= same tap, IP, MAC).
- **Why C3 widening makes this worse**: with the 6 C3 widenings, future-drop between `register_restored` and the final CAS produces the same state without any error path running — no rollback fires at all, the ghost is permanent for the controller's uptime.
- **Action**: either (a) `teardown_restore` also removes the state-map entry (symmetric with `stop_inner`'s `state.write().remove(&sandbox_id)`); or (b) the `LeasedVmSlot` RAII guard (R4-A2) carries an `Arc<RwLock<HashMap>>` reference and removes the entry on Drop unless `disarm()` ran on full success. Option (b) is the structural fix; (a) is the one-line stopgap. **R10's recommendation: ship (a) this cycle as a finite cover; structural (b) via R4-A2.**

### [R10-C2] Rollback `teardown_restore` parks the ntex worker for 10 s (CRITICAL, concurrency-r10)
- **Files**: `crates/sandbox/src/restore_handler.rs:274` (rollback teardown call), `:1135-1150` (`teardown_restore` impl), `:1354-1360` (`nomad_delete_blocking`).
- **Symptom**: R8-A3-5 hopped `submit_restore_job` + `wait_for_livez` through `compio::runtime::spawn_blocking` so the ntex worker can serve other RPCs while Nomad churns. The **rollback** path was missed: `backend.teardown_restore(sandbox_id, snap.vm_index)` at `:274` is a **synchronous** call invoked directly from the async `restore_sandbox`. Internally it calls `nomad_delete_blocking` (sync ureq, 10-second timeout — `:1140`). On every restore failure that propagates through this match arm, the async caller's worker is parked for up to 10 s on the cleanup tail.
- **Race shape**: not strictly a race, but a worker-park. Under c=4 concurrent restores all hitting a rollback (e.g., GCS hiccup on `store.get`), all four ntex workers park simultaneously on `teardown_restore` → no progress for 10 s, surface-level p99 spike.
- **Drop interaction**: Combined with R10-C1, if the future is dropped during this 10-second window the spawn_blocking pool ISN'T involved — the sync `nomad_delete_blocking` runs on the async worker itself; dropping the future cancels the surrounding `do_restore_inner` future BUT NOT the nested sync call, which blocks unwind. Worse: there's no parent future to drop — `teardown_restore` is `fn` not `async fn`. So the rollback isn't even drop-cancellable in the first place.
- **Action**: wrap the rollback teardown the same way `submit_restore_job` was wrapped:
  ```rust
  let backend_for_rollback = Arc::clone(&backend);
  let vm_index_for_rollback = snap.vm_index;
  compio::runtime::spawn_blocking(move || {
      backend_for_rollback.teardown_restore(sandbox_id, vm_index_for_rollback)
  }).await.unwrap_or_else(|p| {
      tracing::error!(panic = ?p, "rollback teardown panicked");
  });
  ```
  Trivial follow-up to R8-A3-5; the deferred slice 6.

### [R10-I1] `do_restore_inner` Drop ordering leaves vm_index reserved + state-map entry on early-Err from final CAS (IMPORTANT, concurrency-r10)
- **Files**: `crates/sandbox/src/restore_handler.rs:600-614`.
- **Symptom**: Sub-case of R10-C1 surfaced as a distinct testable invariant: the `update_sandbox_status(Running)` at `:600` is the LAST possible failure point in `do_restore_inner`. If it returns Err (CasLost is the realistic case post-sweep takeover), the outer match arm correctly calls `teardown_restore` which releases vm_index — BUT the state-map entry installed at `:575-582` is leaked per R10-C1. The pg row in `Restoring`/`Snapshotted` will mismatch the state map.
- **Race shape**: single-future, no concurrent actor — it's the order of operations on the success-tail that doesn't unwind on Err. The clear_snapshot_metadata at `:603` is non-fatal but the question becomes: if `update_sandbox_status` succeeds (running), does clear_snapshot_metadata's failure leak anything? No — that one's logged + ignored. The actionable case is `update_sandbox_status` Err.
- **Action**: subsumed by R10-C1's stopgap (teardown_restore removes state-map entry) or by R4-A2 RAII. Listed separately so it can be unit-tested in isolation: assert that on `update_sandbox_status(Running) → Err(CasLost)`, the state map's `len()` returns to its pre-restore size after the rollback closure runs.

### [R10-M1] Sweep CAS predicate still uses self.host_id() — sweep recovers nothing for crashed peers (MINOR, concurrency-r10, **CARRY-FORWARD from r9 finding 1**)
- **Files**: `crates/sandbox/src/sweep.rs:167-169`, `crates/sandbox/src/db.rs:1690-1697`, `:1773` (CAS WHERE fence).
- **Status**: No code change between r9 and r10 on the sweep CAS path. Still half-installed: C1's `lessee_updated_at` stamp is now present (`de3523c4`), but the CAS still fences `AND host_id = $5::TEXT` against the **sweeping** controller's host_id — which never matches a crashed peer's row.
- **Severity downgrade rationale**: kept MINOR (vs r9's CRITICAL) because the operator-visible impact is still "sweep logs CasLost in a loop; row stays in Restoring" — same as r9. The CRITICAL slot is occupied by the new R10-C1/C2 findings this round; this one didn't move and the proposed fix in r9 (widen `take_dead_host_sandboxes` status filter OR add a `update_lessee`-rewriting UPDATE) stands.

### [R10-M2] spawn_blocking panic-format still renders `Box<dyn Any>` as `Any { .. }` (MINOR, concurrency-r10, **CARRY-FORWARD from r9 finding 7**)
- **Files**: `crates/sandbox/src/restore_handler.rs:406-410, 498, 516`; `crates/sandbox/src/snapshot_handler.rs:366, 377, 401-405`; `crates/sandbox/src/persist.rs:682-685`.
- **Status**: No code change between r9 and r10. Still 4+ duplicated sites of `unwrap_or_else(|p| Err(format!("spawn_blocking panic: {p:?}")))` rendering payload-less `Any { .. }`. Centralize in a `format_spawn_blocking_panic(panic: Box<dyn Any + Send>) -> String` that downcasts to `&str` / `String` first.

## Drop semantics under spawn_blocking — does the wrapping make Drop MORE or LESS dangerous?

Compio's `spawn_blocking` returns a `JoinHandle`-like future. **Dropping that future does NOT abort the blocking closure** — the work runs to completion on the blocking thread pool, the return value is dropped on the floor. This has two implications for the restore handler:

- **Less dangerous than dropping a sync call**: With pre-r8 `submit_restore_job` (sync), a dropped surrounding future would not unwind the in-progress sync call either (it's not a future, can't be cancelled) — but the SURROUNDING future couldn't drop *until* the sync call returned (no yield points). Post-r8 spawn_blocking-wrapped, the surrounding future CAN drop while the spawn_blocking is still running, and the closure keeps going (Nomad job gets submitted) but the result is discarded by the dropped future.
- **More dangerous net effect**: 6 widenings × cancel-unsafe drop windows × no RAII = the side-effect (Nomad alloc, vm_index reservation, state-map insert) lands and the controller has no record. The blocking-pool thread successfully completes its work, then no one consumes the result.

`std::thread::sleep` inside `run_with_timeout` (for `ch-remote` polling) IS cooperative-uncancellable, but is bounded by the deadline + `child.kill()` (`snapshot_handler.rs:622-672`). Bounded by `CH_REMOTE_TIMEOUT = 30s` (`:451`). Acceptable.

**Conclusion**: spawn_blocking made the leak window *easier to enter* (a yield point now exists where none did before) but did not change the magnitude — the side-effects were already not undoable. RAII Drop on a `LeasedVmSlot` is the structural fix; spawn_blocking does NOT block it.

## Carry-forward (open from earlier rounds)

- **[R4-A2]** `LeasedVmSlot` RAII — 5+ cycle open. Would close 6 C3 widenings + R4-A2 + R5-A2 + R8-CONC1 + R9-C1 + R10-C1 + R10-I1. See RAII design sketch below.
- **[C3]** cancel-unsafety in `do_restore_inner` — 6 widenings (r2 → r4 → r5 → r7-C2 → r8-CONC1 → r9-C1). NO new widening this round (no async fence added), but the carry-forward stands.
- **[C1-FOLLOWUP / R10-M1]** sweep CAS still uses `self.host_id()` — half-installed.
- **[R7-C1]** detached teardown task at `admin_handlers.rs:1310-1324` — still no JoinSet / Semaphore / shutdown signal. 3rd cycle open.

## RAII design sketch (concurrency-r10's recommendation for `LeasedVmSlot`)

The prior deferred (C3 entry) hand-waves at "`pin_project` / scope guard". Inspection this round confirms `pin_project` is **not required**: a `LeasedVmSlot` holding only `Arc<…>` + ids is `Unpin` by default (no self-references in the type). The async generator captures it like any other local; its `Drop` runs on cancellation as well as on normal scope exit. No hidden compio pinning incompatibility — the prior deferral was either inertia or a stale assumption.

```rust
// crates/sandbox/src/restore_handler.rs (proposed)

/// RAII guard that owns the post-`reserve_vm_index` /
/// post-`register_restored` cleanup obligations. On Drop without
/// prior `disarm()`, it (a) removes the state-map entry, (b)
/// releases the vm_index, (c) best-effort Nomad DELETE. Mirrors
/// `nomad_ch::CreateGuard` (`backend/nomad_ch.rs:1889-2110`)
/// which already solves the same shape for create.
pub(crate) struct LeasedVmSlot {
    sandbox_id: Uuid,
    vm_index: i16,
    backend: Arc<dyn RestoreBackend>,
    /// Optional handle to the in-memory state map so Drop can
    /// retract the `register_restored` insert. None until the
    /// insert lands; Some(()) after — flipped by `mark_registered`.
    registered: bool,
    armed: bool,
}

impl LeasedVmSlot {
    pub fn new(sandbox_id: Uuid, vm_index: i16, backend: Arc<dyn RestoreBackend>) -> Self {
        Self { sandbox_id, vm_index, backend, registered: false, armed: true }
    }
    pub fn mark_registered(&mut self) { self.registered = true; }
    pub fn disarm(mut self) { self.armed = false; }
}

impl Drop for LeasedVmSlot {
    fn drop(&mut self) {
        if !self.armed { return; }
        // Order: state-map first (synchronous, fast), then
        // teardown_restore (which itself releases vm_index +
        // best-effort Nomad DELETE). teardown_restore runs SYNC
        // here — same caveat as R10-C2; the structural fix is to
        // spawn_blocking the Drop body via compio::runtime::spawn
        // and let it run detached. Acceptable because the Drop
        // already implies "we're unwinding; the surrounding
        // worker is already losing this future's slot."
        if self.registered {
            // Trait method addition: `unregister(sandbox_id)` →
            // calls state.write().remove(&sandbox_id). Default
            // impl no-op so StubRestoreBackend unaffected.
            self.backend.unregister(self.sandbox_id);
        }
        self.backend.teardown_restore(self.sandbox_id, self.vm_index);
    }
}
```

Wire site in `do_restore_inner`:

```rust
backend.reserve_vm_index(snap.vm_index)?;
let mut slot = LeasedVmSlot::new(sandbox_id, snap.vm_index, Arc::clone(&backend));
// ... store.get, rewrite_config_json, submit, wait_for_livez ...
backend.register_restored(...)?;
slot.mark_registered();
// ... clock_resync, update_sandbox_status(Running) ...
slot.disarm();  // success — no Drop cleanup
Ok((vm_index, g2))
```

On any await between `reserve_vm_index` and `disarm`, future-drop unwinds `slot` → Drop runs the cleanup synchronously. The 6 C3 widenings stop mattering: every yield point is now guard-covered.

**Pin-compat verdict**: `LeasedVmSlot` is `Unpin` (no `!Unpin` field). Async-fn local capture works without `pin_project`. The Drop is sync, so the cleanup body must avoid awaits — `teardown_restore`'s sync ureq is fine (it's already on this thread); state-map remove is sync. Drop ordering vs spawn_blocking: if the surrounding future is dropped while a spawn_blocking is in flight, the spawn_blocking completes (its closure owns the cloned `Arc<dyn RestoreBackend>` — separate refcount), but the surrounding generator's `slot` Drop runs **immediately** on the parent thread without waiting for the spawn_blocking to finish. The spawn_blocking's result is discarded; vm_index and state-map are cleaned. Race: if the spawn_blocking just inserted a Nomad job and the Drop's `teardown_restore` runs the nomad DELETE for that same job, we might race the job-create-vs-delete on the Nomad side. Mitigation: `teardown_restore` is idempotent (nomad DELETE returns 404 if not yet created — acceptable; the orphan-prune sweep mops up if needed). Net: safe.

## Status block (one-liner)

```
Round 10:
  NEW: R10-C1 (state-map ghost after rollback), R10-C2 (10s park in rollback teardown), R10-I1 (final-CAS Err ordering).
  CARRIED: R4-A2 (5+ cycles), C3 (6× widened), C1-FOLLOWUP/R10-M1, R7-C1 (3 cycles), R10-M2 (panic format).
  CLEARED: pin-compat concern on LeasedVmSlot (was implicit; explicit now — RAII is structurally available).
```
