# Sandbox/snapshot-restore — concurrency r11 review

Date: 2026-05-25 (UTC)
HEAD at audit: `040812f2`
Round 11 of N. Read-only. Branch `feat/sandbox-snapshot-restore`.

## Summary
- 5 findings (2 critical, 1 important, 2 minor).
- One r10-claim retracted on inspection: **snapshot_handler.rs:424 `vm_ops.teardown_source` is NOT sync-on-async** — production wires `ResolvedSourceVmOps::teardown_source` as an explicit `Ok(())` no-op (both `admin_handlers.rs:1210-1216` and `sweep.rs:423-428`); the real async teardown runs from the admin/sweep layer, not from inside the snapshot handler. R10's r10-C2-sibling concern is invalid as stated. A *different* concern lives at the same line — see [R11-M2] below.
- One pin-compat carry-forward: **R4-A2 `LeasedVmSlot`** still unlanded (6+ cycles, incident-class).
- The R10-C1 + R10-C2 fixes at `be246395` are correct and well-tested (12 regression tests landed). But they introduced two new sub-shapes — surfaced as [R11-C1] (the unregister_restored silent-fail-on-misconfigure) and [R11-C2] (a NEW 2-await rollback window).

## Findings (NEW since r10)

### [R11-C1] `unregister_restored` is gated on `nomad_handle.is_some()` — silently fails the state-map cleanup if a misconfigured prod wiring leaves the handle `None` (CRITICAL, concurrency-r11)

- **Files**: `crates/sandbox/src/restore_handler.rs:1159-1202` (`RealRestoreBackend::teardown_restore`), specifically `:1189-1199` (the gated unregister call). Construction site: `:995-1011` (`RealRestoreBackend::new`) — `nomad_handle: None` by default. Setter: `:1039-1045` (`with_nomad_handle`).
- **Race shape**: not a TOCTOU race per se — a wiring/contract bug whose blast radius is identical to the original R10-C1 ghost. The post-r10 `teardown_restore` does:
  ```rust
  if let Some(handle) = self.nomad_handle.as_ref() {
      let removed = handle.unregister_restored(sandbox_id);
      // ... log if removed ...
  }
  ```
  If `nomad_handle` is `None`, the entire state-map cleanup is skipped silently — the same ghost-entry-after-rollback bug R10-C1 was meant to fix is back. **All other call-sites that need a `nomad_handle` enforce its presence**: `register_restored` at `:1214-1224` returns Err with a loud "B19 wiring missing" message; `derive_agent_url` has no default (per R7-S2's silent-fail-OPEN removal). `teardown_restore` is the only `nomad_handle`-consuming method that silently no-ops on `None`.
- **Production exposure**: today's `AppState::from_config` always plumbs `with_nomad_handle(...)` (B19 wiring landed at `:512-518` of `lib.rs`), so the bad branch is unreachable in current code. But the silent-fail shape is the same R7-S2 hazard the team explicitly removed from `derive_agent_url`: a future refactor that drops the wiring (or a partial constructor in a new code path) reintroduces R10-C1 with no compile-time or runtime signal. The R10-C1 regression tests at `:2186-2266` use the real `nomad_handle` via `with_nomad_handle`; they wouldn't catch a `nomad_handle = None` regression on the prod path.
- **Why critical**: this is exactly the structural debt the prior round flagged — "tiny additive fixes accumulating." R10-C1 was a 1-line stopgap (per R10's own framing: "the structural cure is R4-A2's `LeasedVmSlot` RAII; this 1-line interim is the cheap insurance until that lands"). The stopgap itself ships a footgun; the structural cure is still pending.
- **Action**: either (a) make `nomad_handle = None` a hard error inside `teardown_restore` symmetric with `register_restored` (`tracing::error!` + skip nothing — at least it shows up in logs), or (b) drop the `Option` entirely: `with_nomad_handle` becomes a constructor argument, not a setter, so `nomad_handle: Arc<NomadCHBackend>` is non-optional at the type level. Option (b) is the structural fix and lines up with the LeasedVmSlot sketch from r10. Option (a) is the 2-line stopgap.

### [R11-C2] R10-C2's spawn_blocking wrap introduced a NEW 2-await window in the rollback closure — cancellation between the two awaits leaves the row stuck in `Restoring` despite a fully-completed teardown (CRITICAL, concurrency-r11)

- **Files**: `crates/sandbox/src/restore_handler.rs:291-310` (the post-r10 rollback closure).
- **Pre-r10 shape**: rollback closure had ONE await — `db.update_sandbox_status(...).await` at line 299 — and ONE preceding sync call (`backend.teardown_restore(...)` direct invocation at line 274). A future-drop here would (a) leave the sync teardown's side-effects (Nomad DELETE, state-map remove via R10-C1, vm_index release) AS A SET, since the sync call can't be cancelled; (b) skip the pg row update, leaving the row in `Restoring`.
- **Post-r10 shape**: rollback closure now has TWO awaits:
  1. `spawn_blocking(teardown_restore).await` at `:298` — the join handle.
  2. `db.update_sandbox_status(target, g1, None).await` at `:299-301`.
  Future-drop at any point between the two awaits — e.g., shutdown signal mid-rollback, or the ntex worker dropping the request future because the client disconnected — runs the spawn_blocking closure to completion (compio's spawn_blocking does NOT cancel; its handle's drop discards the result) BUT skips the pg row update. The result is the same wedge-state R10-M1 already flagged for `do_restore_inner`: row stays in `Restoring`, full teardown completed, sweep eventually picks it up via `claim_orphan_transient_for_recovery`.
- **Why critical**: this is a **NEW C3-style widening, in the rollback path itself** — the very path R10-C2 was supposed to make safer. R10's count of "6 widenings" in `do_restore_inner` did not include the rollback closure; the widening is now there too, regressing the cancel-window invariant. Combined with [R11-C1]'s nomad_handle silent-fail, the worst case is: spawn_blocking completes, but the unregister_restored branch in teardown_restore was a no-op (None handle) AND the pg row update was cancelled — so we have a Nomad DELETE'd, vm_index released back into the pool, state-map entry STILL PRESENT, pg row STILL IN `Restoring`. The state map's entry now references a vm_index that's already been reallocated by the next create.
- **Race shape (concrete)**:
  1. T₀: restore handler hits error after `register_restored` succeeded.
  2. T₁: rollback closure enters. `spawn_blocking(teardown_restore)` posted to blocking pool.
  3. T₂: blocking thread runs `teardown_restore`: Nomad DELETE OK, `nomad_handle = None` so unregister_restored skipped (R11-C1 scenario) OR runs (good scenario); `release_vm_index` puts the slot back into the shared allocator. Blocking thread returns; JoinHandle resolves Ok.
  4. T₃: parent future resumes at `:298` await... but the parent future has been DROPPED (client disconnect / shutdown). compio drops the JoinHandle; the side-effects from T₂ have already landed.
  5. T₄: a fresh `create()` arrives on the same controller. The shared allocator hands out the just-released vm_index. New tenant boots on it.
  6. T₅: pg row is still in `Restoring`. The sweep at the `SANDBOX_TRANSIENT_STATE_TIMEOUT_SECS` (default 120s) threshold fires `claim_orphan_transient_for_recovery` → CASes `restoring → snapshotted`. Row is consistent again, but for 120 s the row reads `restoring` while the actual sandbox is gone and the slot has been reused.
- **Action**: same structural fix as R10-C2's: this needs LeasedVmSlot RAII covering the rollback path too, or the rollback closure needs to be cancellation-safe. The 2-line stopgap is `compio::runtime::spawn` (detached) for the entire rollback closure body — but then the request handler returns before rollback even starts, which trades response correctness for cancel-safety. The right answer is RAII (R4-A2).

### [R11-I1] `do_restore_inner` await-count: still 7 (no NEW widening) — but the spawn_blocking awaits are NOT cancel-safe wrt RAII even though they're cancel-safe wrt blocking-pool execution (IMPORTANT, concurrency-r11)

- **Files**: `crates/sandbox/src/restore_handler.rs:426-435, 521, 539, 582, 593, 626, 627`.
- **Count verification** (since R9-C1 reported 7):
  1. `spawn_blocking(store.get).await` — `:429`
  2. `spawn_blocking(submit_restore_job).await` — `:521`
  3. `spawn_blocking(wait_for_livez).await` — `:539`
  4. `persist.unseal(sandbox_id).await` — `:582`
  5. `clock_resync_post_restore(...).await` — `:593`
  6. `db.update_sandbox_status(...).await` — `:626`
  7. `db.clear_snapshot_metadata(...).await` — `:627`
  Still 7. No 8th await added in r10's diff. The rollback-closure additions are OUTSIDE `do_restore_inner`.
- **However**: the spawn_blocking awaits are special. If the parent future is dropped while one is in flight, the blocking-pool work runs to completion (compio's spawn_blocking is uncancellable in the side-effect sense). Awaits 1-3 are therefore "side-effects WILL land if started, but no follow-up code runs." This is actually **worse than a regular await** for the C3 widening argument: a regular await yield can be drop-cancelled and the side-effects don't land; a spawn_blocking yield can be drop-cancelled and the side-effects DO land but no cleanup runs. The R8-A3-5 wrap made worker-park better but cancel-safety worse.
- **Concrete worst-case**: future dropped between `:539` (wait_for_livez join Ok) and `:582` (persist.unseal start). State: vm_index reserved, Nomad alloc running, register_restored not yet called → no state-map entry, no LeasedVmSlot, no rollback fires. Same R5-A2 / R8-CONC1 ghost-alloc shape as before; no new ground but it's worth re-noting that the spawn_blocking wraps moved the failure mode from "worker park, no drop possible" to "drop possible, side-effects always land."
- **Action**: subsumed by R4-A2 (LeasedVmSlot). This is a re-statement, not a new finding, but its severity should bump to IMPORTANT given the rollback closure now also leaks (R11-C2).

### [R11-M1] Sweep recovery CAS is idempotent under retry but NOT idempotent under concurrent peer recovery — first wins, second gets a clean `CasLost` Err (MINOR, concurrency-r11)

- **Files**: `crates/sandbox/src/db.rs:2500-2607`, `crates/sandbox/src/sweep.rs:181-214`.
- **Verification of the question raised in r11 prompt**: Two controllers A and B both observe wedge from crashed C. Both call:
  ```rust
  claim_orphan_transient_for_recovery(sandbox_id, target, row.generation /* G */,
                                      &row.host_id /* C_typed */, threshold_secs)
  ```
  The CAS UPDATE predicate (`:2554-2566`) is:
  - `sandbox_id = $3` (A and B agree),
  - `host_id = $4 = C_typed` (both predicates match initially),
  - `generation = $5 = G` (both match),
  - `status IN ('snapshotting','restoring','restoring_cold')`,
  - `lessee_updated_at IS NOT NULL`,
  - `lessee_updated_at < now() - interval`.
  pg serialises the UPDATEs row-row. A's UPDATE lands first: row becomes `host_id=A_typed, generation=G+1, status=target, lessee_updated_at=NULL`. B's UPDATE then sees the row with `host_id=A_typed ≠ C_typed` → 0 rows matched. B's follow-up SELECT at `:2586-2594` returns the row with `host_id=A_typed, generation=G+1`. B returns `Err(DatabaseError::CasLost { observed_generation: G+1, current_host_id: Some(A_typed) })`. Sweep loop at `:201-205` logs the CasLost as INFO and moves on. ✓ Idempotent under concurrent peer recovery.
- **Retry of A**: A retries with the same `(C_typed, G)`. Row now has `(A_typed, G+1)`. CAS misses on host_id. Same CasLost branch. ✓ Idempotent under self-retry.
- **One sub-shape that IS concerning**: if A's recovery succeeded but A's own update was rolled back at the application layer (e.g., the sweep loop panicked after the UPDATE returned RETURNING but before logging — unlikely, sweep loop has no allocations between `:189` and `:200`), the row still says `host_id=A_typed`. The next sweep tick from A finds the row in non-transient state (`target`), so it doesn't appear in `transient_state_lease_expired_sandboxes` selection at `:138-150`. No wedge. ✓
- **Why minor**: works as designed; this is a verification rather than a finding. The shape is correct.
- **Action**: none. Recovery CAS is concurrency-safe.

### [R11-M2] `vm_ops.teardown_source` at `snapshot_handler.rs:424` is a pure architectural no-op — every production caller wires it as `Ok(())` and runs the real teardown via a separate async path (MINOR, concurrency-r11)

- **Files**: `crates/sandbox/src/snapshot_handler.rs:189-204` (trait), `:424` (sole call), `:718-725` (test stub), `crates/sandbox/src/admin_handlers.rs:1210-1216` (`ResolvedSourceVmOps::teardown_source` impl — explicit `Ok(())`), `crates/sandbox/src/sweep.rs:423-428` (duplicate `ResolvedSourceVmOps::teardown_source` impl — also `Ok(())`).
- **Observation**: R11 prompt asked whether `:424` is sync-on-async. It is sync, BUT both production impls (`admin_handlers::ResolvedSourceVmOps` and `sweep::ResolvedSourceVmOps`) return `Ok(())` unconditionally. The trait method exists but is dead in production — the real teardown runs from `admin_handlers.rs:1311` (`compio::runtime::spawn(...).detach()` on `teardown_source_for_snapshot`) and from `sweep.rs:375-388` (awaited inline on `teardown_source_for_snapshot`).
- **Why this matters for concurrency**: the trait method is a "look-busy" placeholder; the real teardown's concurrency shape lives elsewhere and is governed by R7-C1 (detached spawn in admin path, no JoinSet/Semaphore/shutdown signal). Tests that stub `SourceVmOps` and assert `teardown_called == true` (e.g., the `StubSourceVmOps::teardown_called` Atomic at `:704`) are testing scaffolding that prod has stripped — they don't exercise the real concurrency path. This is a test-vs-prod parity bug, not a runtime concurrency bug.
- **Action**: either (a) collapse the trait method (remove `teardown_source` from `SourceVmOps`, simplify the handler signature), or (b) actually fold the async teardown into the trait via a `SourceVmOps::teardown_source_async` method that the prod impls implement. Option (a) is the simplification cure; option (b) would let the handler observe teardown failures inline. Both are out of scope for a concurrency-only fix; flagging as code-quality with a concurrency-test-coverage angle.

## Closed by recent commits since r10

- **[R10-C1]** (state-map ghost after rollback) — closed at `be246395`. `RealRestoreBackend::teardown_restore` now calls `nomad_handle.as_ref().map(|h| h.unregister_restored(sandbox_id))` (`restore_handler.rs:1189-1199`). 12 new regression tests at `:2186-2426`, plus `nomad_ch.rs:4848-4920`. Symmetric inverse helper `NomadCHBackend::unregister_restored` (`nomad_ch.rs:1706-1712`) lands the `state.write().remove()` call. **BUT**: see [R11-C1] — the fix is gated on `nomad_handle.is_some()`, which is a silent-fail-OPEN footgun if wiring regresses.
- **[R10-C2]** (rollback `teardown_restore` parks ntex worker for 10 s) — closed at `be246395`. Rollback closure at `restore_handler.rs:291-298` now wraps `backend.teardown_restore(...)` in `compio::runtime::spawn_blocking(move || { ... }).await`. spawn_blocking capture is sound: `Arc<dyn RestoreBackend>` is `Send + Sync`, `i16` and `Uuid` are `Copy`. **BUT**: see [R11-C2] — the wrap introduced a new 2-await window in the rollback closure, regressing the cancel-window invariant.

## Carry-forward (urgent, unchanged since r10)

- **[R4-A2]** `LeasedVmSlot` RAII — **6+ cycle open, INCIDENT-CLASS**. R10 explicitly confirmed pin-compat is fine; R10 sketched the type. Would close: 6 C3 widenings in `do_restore_inner` + R4-A2 + R5-A2 + R8-CONC1 + R9-C1 + R10-C1 + R10-I1 + R11-C1 + R11-C2 (the entire family). Every round since r4 has surfaced a new sub-shape of "side-effects landed without RAII cleanup." The structural fix is one type with `Drop`; the additive fixes accumulate per round.
- **[C3]** cancel-unsafety in `do_restore_inner` — still **6 widenings** (no 7th this round). spawn_blocking subtlety: side-effects always land even on future-drop. See [R11-I1].
- **[R10-C2 sibling]** `snapshot_handler.rs:424 vm_ops.teardown_source` — **RETRACTED**. R10 raised this as a potential sync-on-async sibling. On inspection both prod impls return `Ok(())` unconditionally; this is not sync-on-async, it's a code-smell no-op. See [R11-M2].
- **[R7-C1]** detached teardown task at `admin_handlers.rs:1310-1324` — still no JoinSet/Semaphore/shutdown signal. **4 cycles open**. Compounds with [R11-C2]: if the request future drops mid-rollback AND the detached teardown is still running, we now have two concurrent compio tasks operating on the same sandbox_id's state-map with no coordination.
- **[C1-FOLLOWUP / R10-M1]** sweep recovery CAS — verified at [R11-M1]; still half-installed (C1 lessee_updated_at stamp landed, but recovery CAS hasn't been used in the idle-eviction path). The idempotency under concurrent peer is correct.
- **[R10-M2]** spawn_blocking panic-format renders `Any { .. }` — no change. 4+ sites still duplicated.
- **[R10-Q3]** registry.rs 35 bare unwraps on RwLock — **OUT OF SCOPE for concurrency-r11**. Verified: restore_handler / snapshot_handler do not touch `SandboxRegistry`. The unwraps are real (`Grep` confirms 42 `.unwrap()` calls in `registry.rs`, 31+ on RwLock acquisitions), but they're on the preview-URL / sandbox-lifecycle path, not the snapshot/restore path. Pure code-quality, not concurrency-r11.

## Pattern observation — additive-fix accretion

R8: spawn_blocking wrapped 2 critical sync calls.
R9: spawn_blocking wrapped 1 more (clock_resync_post_restore).
R10-C1: 1-line state-map cleanup added to teardown_restore (gated on nomad_handle).
R10-C2: spawn_blocking wrapped rollback teardown.
R11-C1: the R10-C1 gate is a silent-fail-OPEN.
R11-C2: the R10-C2 wrap introduced a new 2-await window in rollback.

Every additive fix shipped this round either (a) inherited the same silent-fail shape that R7-S2 explicitly removed, or (b) introduced a new yield point that the structural RAII would have covered. The marginal cost of "one more cycle of additive fixes" is now negative: each fix surfaces ≥1 new shape that LeasedVmSlot would have covered. **Per r10's own framing**: the LeasedVmSlot RAII is structurally available (pin-compat cleared, type sketched). Continuing to add per-call-site spawn_blocking wraps + per-field nomad_handle gates is paying interest on a debt that R4-A2 would retire in one PR.

## Status block (one-liner)

```
Round 11:
  NEW: R11-C1 (unregister_restored silent-fail on nomad_handle=None),
       R11-C2 (rollback closure now has 2-await window),
       R11-I1 (await-count still 7 but spawn_blocking cancel-semantics),
       R11-M1 (sweep recovery CAS idempotency verified — correct),
       R11-M2 (vm_ops.teardown_source is dead-in-prod no-op).
  CLOSED-AND-VERIFIED: R10-C1 (state-map ghost) at be246395,
                      R10-C2 (rollback worker-park) at be246395.
  CLOSED-WITH-CAVEAT: R10-C1 fix gated on nomad_handle (see R11-C1).
                     R10-C2 fix introduced new C3-widening (see R11-C2).
  RETRACTED: r10's "C2 sibling" at snapshot_handler.rs:424 (no-op in prod).
  CARRIED: R4-A2 (6+ cycles, incident-class — would close 9 findings),
           C3 (6 widenings, no 7th this round; cancel-semantics nuance),
           R7-C1 (detached teardown, 4 cycles).
```
