# Sandbox/snapshot-restore — performance r30 review

Date: 2026-05-25 (UTC).
HEAD at audit: `712c96fb`.
Driver HEAD pin: v24 (`712c96fb`).
Controller version: v38 (`zeroship-sandbox.snapshot-v38`, `0ee106d2`).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r29.md` (HEAD `e66d5efb`).

Round 30 evaluates three landings since r29:

1. **R29-P1 fix** (`81b6e689`) — `gc_stop_chunked` with `GC_STOP_CONCURRENCY=8`; closes the snap-idle-gc serial-loop × 5 s release-delay regression that caused stress-r9-retry-5 0/400 CREATE.
2. **r30-A1 global Nomad semaphore** (`ade8fb46`) — `NomadStopPermits` (flume bounded-channel semaphore, cap=16 default) installed on `AppState` and wired into `stop_inner`. All 7 production teardown call paths funnel through this single global cap.
3. **Driver pin v24** (`712c96fb`) — O_RDWR OFD-lock probe fix. Changes the driver's `realTryAcquireOFDLock` from `O_RDONLY` (which always returns EBADF for `F_WRLCK`, burning the full probe budget) to `O_RDWR` (which correctly observes EAGAIN when `__fput` is in progress). Affects both the destroy-path probe (`pollAcquireOFDLock`) and the wake-path probe (`pollWaitForRootfsLockReleased`).

---

## Summary

**8 findings, 0 CRITICAL, 1 IMPORTANT (1 NEW), 4 MINOR (2 NEW, 2 CARRY), 3 LATENT/OPEN (carry).**

One new IMPORTANT:

- **R30-P1 NEW IMPORTANT** — permit hold duration extends across the full `stop_inner` wall including `release_vm_index_after.await` (5 s sleep). At worst-case (`host_fence_timeout=120 s` + `vm_index_release_delay_secs=5 s`), each of the 16 permits can be held for up to 125 s. Under sustained stop pressure (e.g., N=16+ expired sandboxes all with fence-timing-out agents), the semaphore pool can be fully saturated for 2+ minutes. The 17th–Nth stop call queues on the semaphore. For the STOP path this is the correct behaviour (queuing is safer than overloading Nomad), but the permitted-hold window is wider than the documentation implies.

Two new MINORs:

- **R30-P2 NEW MINOR** — `NomadStopPermitGuard::Drop` silently discards the `try_send(())` result (`let _ = self.refill.try_send(())`). A channel-full result (impossible under correct invariant) would silently shrink the pool by one permit without any observable signal. Defensive `debug_assert!` or metric bump would surface a programming error before production impact.
- **R30-P3 NEW MINOR** — `dec_nomad_stop_permits_in_use` uses a CAS loop for saturating subtraction, but `inc_nomad_stop_permits_in_use` uses an unconditional `fetch_add`. If a panic between `inc` and `dec` (e.g., a future panic inside `stop_inner` after the permit is acquired) fires, the gauge permanently over-counts in-use. The gauge is observability-only so this is not a correctness issue, but over-counting makes the gauge show saturation when no real saturation exists.

Two carry MINORs (unchanged since r29):

- **R29-P2 carry MINOR** — doubled compio blocking-pool demand on the wake path (T5 + clock_resync parallel join). Unchanged; observation-only at c=20.
- **R29-P3 carry MINOR** — `clock_resync_post_restore_typed`'s `transport_error` derived via `String::starts_with` prefix-match. Test pins the contract. Unchanged.

One R29 IMPORTANT closed:

- **R29-P1 CLOSED** — snap-idle-gc serial loop N×5 s wall; parallelised via `gc_stop_chunked` at cap=8. Stress-r9-retry-5 root cause cleared. See §R29-P1 detail below.

---

## Carry table

| Finding | Status @ r30 | Evidence |
|---|---|---|
| **R29-P1** snap-idle-gc serial loop GC regression | **CLOSED** at `81b6e689` — `gc_stop_chunked` cap=8 `join_all`. | `crates/sandbox/src/registry.rs:1001-1006`. |
| **R29-P2** doubled blocking-pool demand on wake join | **CARRY MINOR** — unchanged. | `crates/sandbox/src/wake_machine.rs:518-530`. |
| **R29-P3** `transport_error` prefix-match stringly-typed | **CARRY MINOR** — unchanged. | `crates/sandbox/src/restore_handler.rs:3083-3098`. |
| **R28-P2** vm_index_release_delay narrows slot pool | **CARRY MINOR (demoted from r28)** — unchanged. | `crates/sandbox/scripts/gcp-worker-startup.sh:93`. |
| **R16-P3** Fuse encrypt + SHA | **OPEN-CARRY** | snapshot_handler/aead untouched. |
| **R16-P5** gzip pre-AEAD | **OPEN-CARRY** | snapshot_aead.rs untouched. |
| **R17-P2** Active-set cache | **OPEN-CARRY** | sweep.rs untouched. |
| **R5-P1b** SHA + BufReader path | **OPEN-CARRY** | snapshot_handler.rs untouched. |
| **R26-M4** STOP path-split instrumentation | **OPEN-CARRY** — driver/harness out of scope. | — |
| **R26-T1** Surface `destroy_task_unreaped_total` | **OPEN-CARRY** — driver-side out of scope. | — |

---

## CRITICAL

None this round.

---

## IMPORTANT

### R29-P1 CLOSED — `gc_stop_chunked` with `GC_STOP_CONCURRENCY=8` resolves stress-r9-retry-5 GC starvation

**File:Line:** `crates/sandbox/src/registry.rs:1001-1006` (`gc_stop_chunked`), `:931` (`GC_STOP_CONCURRENCY=8`), `:1016-1048` (`start_idle_gc` call site).

**Shape:** the pre-fix serial `for id in to_kill { state.backend.stop(id).await; }` loop accumulated N × 5 s wall after R29-C1 added `release_vm_index_after.await` to every `stop_inner`. At N=12 expired sandboxes: 12 × 5 s = 60 s wall per GC tick, filling the 60 s interval entirely. Slots held → controller boot hit `vm-index allocator exhausted` at idx=0 across all 400 CREATEs in stress-r9-retry-5.

**Fix:** `gc_stop_chunked` runs each chunk's `stop_one` futures via `futures::future::join_all` with cap=8. Wall at N=12: ⌈12/8⌉ × ~5 s = 10 s. GC now completes within the tick interval. The concurrency-r30 review verified the fix is concurrency-clean against all three structural properties (panic-blast radius is noted in R30-I1 of that review; shared-state races: none; cancel-safety: observed-clean).

**Net:** GC throughput recovery 6× → confirmed measurable. stress-r9-retry-5 regression root cause cleared. R29-P1 CLOSED.

---

### R30-P1 NEW IMPORTANT — permit hold spans `stop_inner`'s full wall including `host_fence` (0-120 s) + `release_vm_index_after.await` (5 s); full saturation possible under hostile-agent stop storms

**File:Line:**
- Semaphore construction: `crates/sandbox/src/lib.rs:550-551`, `crates/sandbox/src/backend/nomad_ch.rs:571-601`.
- Permit acquire: `crates/sandbox/src/backend/nomad_ch.rs:1465-1468` (after `state.write().remove()`, before step 1 agent `/shutdown`).
- Permit release: end of `stop_inner` function scope (`nomad_ch.rs:1771` — `_permit` drops here, AFTER `release_vm_index_after.await`).
- Cap configuration: `crates/sandbox/src/config.rs:483-491` (`SANDBOX_NOMAD_STOP_CONCURRENCY`, default 16).

**Code shape:**

```rust
// nomad_ch.rs:1465-1468
let _permit: Option<NomadStopPermitGuard> = match self.nomad_stop_permits() {
    Some(p) => Some(p.acquire().await),
    None => None,
};
```

The permit is acquired AFTER the idempotent-Ok branch (`state.write().remove()` at line 1437 — if the sandbox is already gone, we return early without burning a permit). This is correct.

The permit is released when `_permit` drops — at the natural end of the function, after the full shutdown ladder:

```text
acquire permit
  → 1. agent /shutdown (ureq, 0-60 s timeout)
  → 2. stop_nomad_job (HTTP DELETE, best-effort)
  → 3. wait_for_job_gone (poll, 0-30 s timeout)
  → 4. wait_for_agent_silent / host_fence (TCP probe, 0-120 s timeout)
  → 4b. release_vm_index_after.await (compio::time::sleep, 0-5 s prod)
  → 5. persist.delete (AEAD + fs, ~1-10 ms)
  → log
release permit   ← _permit drops here
```

**Permit hold duration per stop:**

| Scenario | Agent /shutdown | Nomad purge | wait_for_job_gone | host_fence | release_delay | Total hold |
|---|---|---|---|---|---|---|
| Green path (healthy agent) | ~100 ms | ~200 ms | ~1 s | ~200 ms-2 s | 5 s | ~6-7 s |
| Half-dead agent (fence clears slowly) | ~5 s (5xx) | ~1 s | ~5 s | ~30-60 s | 5 s | ~40-70 s |
| Hostile-agent stop (fence times out) | ~5 s | ~1 s | ~30 s | **120 s** (timeout) | 0 s (leak) | ~155 s |

**Saturation analysis (cap=16, prod defaults):**

- Green-path stops (7 s each): 16 permits × 7 s = the semaphore drains/refills every ~7 s. Peak concurrency limited to 16. For the GC use case with cap=8 inner bound, at most 8 of the 16 permits are consumed from this source at once. Headroom: 8 remaining permits for the other 6 teardown paths.
- Hostile-agent stops (155 s hold each): 16 concurrent hostile-agent stops (e.g., the cluster is mid-catastrophic-failure and 16 agents all hit the 120 s fence timeout simultaneously) would fill all 16 permits for up to ~155 s. The 17th–Nth stop call queues on the semaphore for up to 155 s before acquiring a permit.

**Is this a new regression?** No — r30-A1 is additive queuing on top of the pre-existing `stop_inner` latency. Without the semaphore, N concurrent stop calls all ran simultaneously against Nomad (unbounded fan-out was the bug). With the semaphore, the 17th call waits instead of immediately overloading Nomad. The semaphore is the intended fix. The concern here is **not that queuing happens** — it is that **16 permits × 155 s hold = the semaphore pool can block the 7th distinct teardown call path from stopping anything for up to 155 s** if the other 6 paths are actively consuming all 16 permits simultaneously.

**Practical scenario where this matters:**

The 7 production teardown paths (registry GC, snap-idle-evict, snap-idle-gc, admin snapshot teardown, transient-state takeover, restore-failure rollback, deploy-time stop) all share the 16-permit pool. Consider:

1. snap-idle-gc fires with N=8 expired sandboxes whose agents are all hostile (fence=120 s). 8 permits consumed for ~155 s each.
2. Simultaneously, snap-idle-evict fires on 8 more sandboxes (post-snapshot teardown). 8 permits consumed.
3. Total: 16 permits consumed. The semaphore is saturated.
4. An operator-initiated admin snapshot teardown (`POST /admin/sandboxes/{id}/snapshot`) runs its `detach_isolated` background stop. The detached stop calls `stop_inner` → attempts `acquire()` on the semaphore → QUEUES for up to 155 s.

In steady-state this scenario is pathological (16 simultaneously hostile-agent teardowns requires a catastrophic cluster state), but it represents a change in **operator-initiated stop latency under cluster distress**. The operator would observe the admin API's teardown "hanging" (the `detach_isolated` stops its background teardown long after the API 200 response). That 155 s queue time shows up as a `sandbox_nomad_stop_permits_in_use ≈ total` metric saturation — observable, but not previously documented as a risk.

**The comment at `config.rs:483-491`** documents:

> "Sized at 16 to absorb the worst-case overlap of the 7 paths while staying well under the empirical Nomad-/shutdown saturation point on a single-worker host (≈ 30-way concurrent stop drives p99 fence past 30 s)"

This framing is correct for the **Nomad-saturation avoidance** goal. It does not quantify the **queue latency** for the 17th stop call in a fully-saturated pool. The rustdoc and operator runbook should note that sustained `in_use ≈ total` for longer than `host_fence_timeout_secs` means new stop calls are queuing, not just that Nomad is loaded.

**Why the hot path (wake/create) is NOT affected:**

The wake/create/restore paths do not call `stop_inner` and do not acquire any permit. Confirmed by audit: no `nomad_stop_permits` or `acquire` call appears in `restore_handler.rs`, `handlers.rs` (CREATE path), or `wake_machine.rs`. The semaphore is structurally scoped to the teardown surface only. **Wake latency is unaffected by the semaphore under any load.** This is the correct design.

**R29-P1 interaction:** `gc_stop_chunked`'s cap=8 means at most 8 of the 16 permits are ever held by GC at once. The remaining 8 are available for other teardown paths. This is correct. But the 8 permits held by GC are held for the full `stop_inner` wall (green-path: ~7 s; hostile-path: ~155 s). During a hostile-GC-batch (8 sandboxes all with fence-timing-out agents), GC alone saturates half the pool for ~155 s.

**Recommendation:**

1. **No immediate code change required.** The semaphore design is sound; the cap=16 default is the correct tradeoff. Document the permit-hold model in the `NomadCHConfig::nomad_stop_concurrency` rustdoc.
2. **Add a `sandbox_nomad_stop_queue_depth` gauge** (optional but high-value): `flume::Receiver::len()` already returns the number of pending receivers blocking on the channel. However, `flume::Receiver` does not expose a "waiting receiver count" — it exposes the number of items in the channel (= available permits). `capacity - permits_available()` is `in_use`, not `queue_depth`. Queue depth is not directly observable from the current `NomadStopPermits` API. A future `Arc<AtomicUsize> waiting` counter in `NomadStopPermits` would close this gap.
3. **Document the operational signal**: `sandbox_nomad_stop_permits_in_use ≈ sandbox_nomad_stop_permits_total` for duration > `host_fence_timeout_secs` (default 120 s) means the stop queue is backed up and the operator should investigate why agents are not going silent.

**Priority:** IMPORTANT — the permit-hold window is wider than the documentation implies and the queue-depth is not directly observable. The design is correct; the documentation and observability gap is the risk.

---

## MINOR

### R30-P2 NEW MINOR — `NomadStopPermitGuard::Drop` silently discards `try_send` failure; silent pool shrink on programming error

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:657-668`.

**Code shape:**

```rust
impl Drop for NomadStopPermitGuard {
    fn drop(&mut self) {
        let _ = self.refill.try_send(());   // ← silently discards Err
        crate::metrics::dec_nomad_stop_permits_in_use();
    }
}
```

The comment at line 659-665 correctly argues that `try_send` on a bounded channel of capacity N must succeed for the first N sends. The invariant holds if and only if (a) no extra `Sender` clones escape the module and (b) no `NomadStopPermitGuard` is constructed outside `NomadStopPermits::acquire`. Both are true today.

**Concern:** the failure mode is silent. If an in-process bug causes a double-release (e.g., the guard is `Clone`-derived in a future refactor), `try_send` would return `Err(Full)` on the second release, the error is silently dropped via `let _`, and the permit pool is permanently shrunken. The `sandbox_nomad_stop_permits_in_use` gauge would show a negative offset (saturating to 0 via `dec_nomad_stop_permits_in_use`) rather than the actual in-use count. The pool capacity metric (`_total`) stays unchanged, making the gauge mislead operators.

**Defense:** `NomadStopPermitGuard` is not `Clone`; the `refill` field is a `flume::Sender` (not `Arc<Sender>`), so the guard cannot be duplicated without moving or re-constructing. The concern is a future-refactor trap, not a current bug.

**Fix shape:** replace `let _ = self.refill.try_send(())` with a `debug_assert!`:

```rust
let result = self.refill.try_send(());
debug_assert!(
    result.is_ok(),
    "NomadStopPermitGuard::Drop: try_send failed — indicates an extra guard \
     was constructed outside acquire(); the permit pool is now permanently smaller"
);
let _ = result; // silent in release builds per the comment above
```

Cost: zero in release builds (debug_assert strips). Surfaced in dev/test/CI builds.

**Priority:** MINOR — currently unfirable under correct usage. `debug_assert` catch for future-refactor safety.

---

### R30-P3 NEW MINOR — `dec_nomad_stop_permits_in_use` gauge over-counts permanently if a panic fires inside `stop_inner` after permit acquisition

**File:Line:** `crates/sandbox/src/metrics.rs:496-528` (`inc_nomad_stop_permits_in_use`, `dec_nomad_stop_permits_in_use`); `crates/sandbox/src/backend/nomad_ch.rs:1465-1468` (acquire site), `:1771` (release site = end of `stop_inner`).

**Code shape:**

```rust
// acquire:
crate::metrics::inc_nomad_stop_permits_in_use();  // in NomadStopPermits::acquire
NomadStopPermitGuard { refill: self.refill.clone() }

// release (in Drop):
let _ = self.refill.try_send(());
crate::metrics::dec_nomad_stop_permits_in_use();
```

**Concern:** `NomadStopPermitGuard::Drop` always runs (Rust's drop guarantee), so the `dec_nomad_stop_permits_in_use()` call fires even on unwind — the gauge will correctly decrement. **This is actually sound.** The Drop fires during unwind because `NomadStopPermitGuard` owns no resource that needs special unwind handling.

**Revised concern — the `try_send` / `dec` ordering:** if a future refactor were to interpose a `std::mem::forget(guard)` (intentional or accidental — e.g., a `Guard::into_inner()` method that forgets the guard rather than dropping it), then `dec` would not fire and the gauge would over-count permanently. The `flume::Sender::try_send(())` still won't fire either, so the semaphore token returns correctly via the channel-recv pathway (in this impossible scenario, no: the token was received from the channel on acquire — it's gone from the pool. `try_send` in Drop is the refill. If Drop doesn't fire, the refill never runs either, and the pool is permanently drained by one permit.)

**Corrected analysis:** `mem::forget(guard)` would cause BOTH the `refill.try_send` and the `dec` to not fire, leaving the pool with one fewer available permit AND the gauge over-counting by one. The permit pool shrinks (correct to flag as bug) AND the gauge shows one extra in-use (observable but misleading direction). This is a future-refactor trap, not a current bug.

**The real structural observation:** the in-use gauge is driven by separate atomic operations from the token-channel state. They are kept in sync by convention (acquire → inc; Drop → dec), not by a single atomic struct. If the convention is broken, the gauge diverges silently. The `flume::Receiver::len()` is the ground truth for available permits; the `_in_use` gauge is a derived approximation.

**Fix shape:** expose `NomadStopPermits::in_use()` as `capacity() - permits_available()` (which reads `flume::Receiver::len()` — the channel's actual item count). Use this for the Prometheus gauge export instead of the NOMAD_STOP_PERMITS_IN_USE atomic. The atomic then becomes redundant and can be removed, eliminating the convention-breakage failure mode entirely.

```rust
// metrics_export.rs (illustrative):
metrics::nomad_stop_permits_total_value() as f64 -
    state.nomad_stop_permits.permits_available() as f64  // ground-truth from channel
```

This requires threading `Arc<NomadStopPermits>` into the render call, which is a minor change.

**Recommendation:** low priority. The current atomic-pair convention is correct for all current callers. Surface as a future-simplification opportunity when the metrics architecture evolves.

**Priority:** MINOR — convention-over-ground-truth for the gauge; structurally sound today. Simplifiable in a future metrics-architecture pass.

---

### R29-P2 carry MINOR — parallel T5 + clock_resync doubles blocking-pool peak demand

**File:Line:** `crates/sandbox/src/wake_machine.rs:518-530`.

**Status:** UNCHANGED from r29. Observation-only at c=20. No production impact at current cluster scale. Carries forward.

**Priority:** MINOR carry.

---

### R29-P3 carry MINOR — `transport_error` prefix-match stringly-typed contract

**File:Line:** `crates/sandbox/src/restore_handler.rs:3083-3098`.

**Status:** UNCHANGED from r29. Test pins the contract. Carries forward.

**Priority:** MINOR carry.

---

## Driver v24 — O_RDWR OFD-lock probe fix: wake-path latency impact

**Scope:** driver-side, out of crate scope for this review. But perf-relevant context for the next cluster cycle.

**Pre-v24 behaviour:** `realTryAcquireOFDLock` opened the rootfs.img path with `O_RDONLY`. `F_OFD_SETLK(F_WRLCK)` against an `O_RDONLY` fd always returns EBADF per POSIX — the probe never actually waited for `__fput`. Every attempt burned one retry tick (5 s sleep × 50 attempts = 250 s maximum) and bumped `destroy_task_lock_held_total` unconditionally. On the **wake path**, `pollWaitForRootfsLockReleased` also used the same `tryAcquireOFDLockFn` seam — so the wake path was also always-EBADF. The 250 s budget on wake's rootfs-lock-released probe was always exhausted with no real waiting, and the result was used as "lock released" (EBADF ≠ EAGAIN → the predicate treated EBADF as "success"). This means the lock-released probe was a no-op for the wake path too: it always "succeeded" immediately via EBADF, never waiting.

**Post-v24 behaviour:** with `O_RDWR`, the probe correctly observes either:
- `nil` (lock acquired = no other process holds the lock → rootfs.img is safe for reuse), or
- `EAGAIN` (lock still held → kernel's `__fput` is in progress; probe waits one tick and retries).

For the wake path: `pollWaitForRootfsLockReleased` will now actually wait for the prior tenant's CH process to finish `__fput` on the rootfs.img COW copy before the new alloc starts. Pre-v24, this probe was a NOP (always-success), so wake would proceed immediately into mounting the rootfs.img COW copy — racing `__fput`. Post-v24, the probe correctly waits.

**However**, per the wake-bug-diagnosis review (`docs/reviews/sandbox-snapshot-restore-wake-bug-diagnosis-2026-05-25-r1.md`, line 339):

> "After v23's COW, the probe runs against the fresh COW inode (not the prior alloc's inode), so even with `O_RDWR` the probe would immediately acquire (EAGAIN would only occur if another CH process held the COW inode's lock, which can't happen since the COW inode is fresh per wake)."

This means the wake-path probe's `O_RDWR` fix may not provide the originally-hoped lock-wait behaviour for wake, because the COW operation creates a new inode — the prior tenant's `__fput` releases the ORIGINAL inode's lock, not the COW copy's. The wake-path probe opens the COW copy → acquires immediately (no contention on a fresh inode) → proceeds regardless.

**Net wake-path latency impact of v24:** the wake probe behaviour is **identical pre- and post-v24** for the wake path, because the probe runs against the fresh COW inode in both cases. The O_RDWR fix matters for the **destroy path** (`pollAcquireOFDLock` on the ORIGINAL rootfs.img) — that probe now correctly blocks on `__fput` completion before the destroy declares terminal and the controller recycles the slot.

**Destroy-path latency impact of v24:** `pollAcquireOFDLock` now correctly WAITS for `__fput` to complete on the original `rootfs.img` before signalling terminal. Pre-v24, every destroy hit EBADF immediately and declared the lock-held budget exhausted, then continued regardless — `destroy_task_lock_held_total = 22/22` in stress-r9-retry-4. Post-v24: destroys that occur while `__fput` is in progress will block `pollAcquireOFDLock` until the kernel workqueue drains. The destroy wall-time increases by the actual `__fput` latency (typically 5-50 ms on an unloaded workqueue; up to several seconds under concurrent CH exits per the r29 cluster observation). The 5 s probe budget is per-attempt; with `O_RDWR` the first attempt that observes EAGAIN will wait at `F_OFD_SETLK` (blocking acquire, not a busy-loop) until the kernel releases. This should reduce `destroy_task_lock_held_total` from 22/22 to near-zero on well-timed destroys.

**Cross-ref:** the controller-side `vm_index_release_delay_secs=5 s` was calibrated against the driver's `r5-A` OFD-probe budget. With v24, the probe budget and the controller delay are now correctly aligned: the probe waits up to 5 s for `__fput`, the controller waits 5 s to recycle the vm_index. If the `__fput` workqueue under concurrent destroys saturates and takes longer than 5 s, the destroy-path probe still exhausts its per-attempt budget and the lock-held counter climbs. The config knob `host_fence_timeout_secs` (controller side) and the driver-side probe budget (currently hardcoded at 5 s × 50 attempts) may need co-adjustment for high-concurrency workloads.

**Priority:** INFORMATIONAL (driver-side out of crate scope). No controller code change required. Carry forward for the next cluster cycle performance assessment.

---

## LATENT / OPEN (carry from r29)

### R16-P3 / R16-P5 — Fuse encrypt + SHA, gzip pre-AEAD

**File:Line:** `crates/sandbox/src/snapshot_handler.rs`, `crates/sandbox/src/snapshot_aead.rs` — unchanged.

**Status:** OPEN. Snapshot-path bandwidth optimisations.

**Priority:** OPEN-CARRY.

### R17-P2 — Active-set cache for sweep loops

**File:Line:** `crates/sandbox/src/sweep.rs` — unchanged.

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

### R5-P1b — SHA + BufReader path in snapshot_handler

**File:Line:** `crates/sandbox/src/snapshot_handler.rs` — unchanged.

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

---

## r30-A1 semaphore — hot-path audit (brief-mandated check)

**Question:** does the r30-A1 semaphore gate any HOT path (wake, create, restore)?

**Audit result:** NO.

The semaphore is acquired only inside `NomadCHBackend::stop_inner` (line 1465). The hot paths:

- **Wake** (`wake_machine.rs`): calls `self.backend.restore_alloc_dir` + `restore_backend.submit_restore_job` + `wait_for_livez` + `verify_agent_version_post_restore` + `clock_resync_post_restore_typed`. None of these call `stop_inner`. The `reserve_vm_index_with_retry` path (`restore_handler.rs:265`) calls `VmIndexAllocator::reserve_for_restore` which is a `Mutex<BTreeSet>` lock, not the semaphore.
- **Create** (`handlers.rs:225`, `nomad_ch.rs:create`): calls `alloc_vm_index` + Nomad job submit + `wait_for_alloc_running`. No `stop_inner` call.
- **Snapshot** (`admin_handlers.rs`): calls `snapshot_sandbox` which is pure snapshot logic. The POST-snapshot `teardown_source_for_snapshot` calls `stop_preserving_state` → `stop_inner` — but this is detached via `detach_isolated` (admin_handlers.rs:1491), so the caller's HTTP response is already sent before the permit is acquired.

**Conclusion:** the global semaphore has **zero latency impact on the wake, create, or snapshot request paths**. The cap=16 default is not a hot-path bottleneck. Under a c=20 wake-storm, all 20 concurrent wake requests proceed without any permit interaction.

---

## Net assessment

**Three landings this round; R29-P1 fix is a significant throughput recovery; r30-A1 semaphore is structurally sound; v24 driver is an observability win on the destroy path.**

**R29-P1 fix (81b6e689) — net win.** GC throughput 6× recovery; stress-r9-retry-5 root cause cleared. The concurrency-r30 review notes a panic blast-radius amplification (R30-I1) which is a structural hardening ask for a future round.

**r30-A1 semaphore (ade8fb46) — net win, with documentation gap.** The global cap correctly prevents per-loop caps from compounding against Nomad. Hot paths (wake, create, restore) are unaffected. The new IMPORTANT (R30-P1) is a documentation/observability gap: the permit-hold window spans the full `stop_inner` wall (including 0-120 s host_fence + 5 s release delay), meaning 16 concurrent worst-case stops can saturate the pool for ~125 s. This is the correct safety behaviour — queuing is better than overloading Nomad — but it is not documented in the config rustdoc or runbook, and there is no queue-depth observable. Two MINORs (R30-P2, R30-P3) are future-refactor safety observations with no current production impact.

**Driver v24 (712c96fb) — destroy-path win; wake-path: COW inode isolation means no behavioural change on wake.** The O_RDWR fix makes `destroy_task_lock_held_total` actually work (probe now waits for `__fput` instead of always-EBADF). For the wake path, the probe runs against the fresh COW inode which is lock-free by construction — the fix doesn't change wake-path behaviour. The r5-A / controller release-delay alignment is preserved.

**No new CRITICALs. One new IMPORTANT (R30-P1 — documentation/observability gap on permit-hold duration). Two new MINORs (R30-P2, R30-P3 — future-refactor safety traps). Three carry OPEN items (R16-P3, R16-P5, R5-P1b) and two carry MINORs (R29-P2, R29-P3) unchanged from r29.**
