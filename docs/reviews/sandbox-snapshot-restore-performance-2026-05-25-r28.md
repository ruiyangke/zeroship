# Sandbox/snapshot-restore — performance r28 review

Date: 2026-05-25 (UTC).
HEAD at audit: `a3cfca10`.
Driver HEAD pin (per `gcp-worker-startup.sh`): v19 (`086971d2`).
Controller version: v36 (`zeroship-sandbox.snapshot-v36`).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r27.md` (HEAD `568c1357`).
Cluster signal: T-8b-stress-r9 RED (0/400 e2e CREATE) — `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r9.md`. **Upstream of r24-A2 — STARTUP-HEREDOC-LEAK at `gcp-worker-startup.sh:511` prevented the v19 ch driver from loading on any worker. r24-A2-S2+S3 efficacy UNKNOWN.** Heredoc fix landed at `a3cfca10`; STRESS-R9-RETRY-2 pending.

Round 28 evaluates the two new perf-affecting fixes that landed since r27 — r24-A2-S3 (`c969b94d`) 5-second VmIndexAllocator release delay on every stop/create-rollback, and R28-C1 (`9e1f6276`) inline-sleep restructuring of `CreateGuard::drop`. Plus carry triage on the r27-open backlog.

## Summary

**11 findings, 0 CRITICAL, 1 IMPORTANT (1 NEW), 5 MINOR (3 NEW, 2 CARRY), 5 LATENT/OPEN (carry).**

Two perf-affecting landings this round:
- **R28-C1 fix at `9e1f6276`** — inline `sleep(release_delay).await` + `vm_index_allocator.lock().release(i)` replaces the `spawn_delayed_release` helper call inside `CreateGuard::drop`'s cleanup future (which runs on the short-lived `detach_isolated("create-rollbk", …)` runtime). The fix blocks the dedicated OS-thread + private compio runtime for `release_delay` seconds (production default 5 s) after Nomad purge confirms. Operator-visible at create-failure rollback only — happy-path CREATE never fires `CreateGuard::drop`. **Acceptable: see R28-P1 below.**
- **r24-A2-S3 at `c969b94d`** — `VmIndexAllocator::spawn_delayed_release` helper + 5 s production default. Adds 5 s to every stop's index-reclaim path. Index pressure at 12-slot ceiling × 3 workers (= 36 slots fleet-wide) flagged in the task brief; **see R28-P2 below for the math.**

One new IMPORTANT:
- **R28-P2 NEW IMPORTANT** — the 5 s `vm_index_release_delay_secs` default narrows the per-worker steady-state slot pool from 12 to ~7-8 under sustained burst at the smoke-r4 wake p50 of 45.6 s. The 4-slot reserve absorbs the delay but trims headroom; at stress c=20 burst against 3 workers (ceiling=12) the per-worker working set is at 60-80% of the ceiling rather than the 40-60% the brief assumes. Real risk is at restore-storm scenarios where multiple wakes land before any stop releases.

One carry escalated:
- **R27-P1 carry IMPORTANT** — T5 `/version` probe still serial with `clock_resync_post_restore` (no parallelize landed). T-8b-stress-r9 RED upstream-of-driver-load → no cluster signal on T5 effect. Carries forward unchanged.

## Carry table

| Finding | Status @ r28 | Evidence |
|---|---|---|
| **R27-P1** T5 `/version` serial with clock_resync | **CARRY IMPORTANT** — no work landed; stress-r9 didn't exercise the wake path (driver never loaded). | `crates/sandbox/src/wake_machine.rs:485-548`, `crates/sandbox/src/restore_handler.rs:3093-3273`. Unchanged since r27. |
| **R26-C1** thread-local pg pool | **CLOSED** at `ee702d5f` + `8c0b361e`. | `crates/sandbox/src/db.rs:617-642, 657-672` — unchanged since r27. |
| **R26-I2** `try_create` spawn_blocking | **CLOSED** at `73725aa3`. | `crates/sandbox/src/backend/nomad_ch.rs:842-866` — unchanged since r27. |
| **R16-P3** Fuse encrypt + SHA | **OPEN** — no work landed since r16. | snapshot_handler/aead crate untouched. |
| **R16-P5** gzip pre-AEAD | **OPEN** — no work landed since r16. | snapshot_aead.rs untouched. |
| **R17-P2** Active-set cache | **OPEN** — no work landed since r17. | sweep.rs untouched. |
| **R5-P1b** SHA + BufReader path | **OPEN** — no work landed since r5. | snapshot_handler.rs untouched. |
| **R26-M4** STOP path-split instrumentation | **OPEN** — no harness change. | (driver-side / harness; out of crate scope.) |
| **R26-T1** Surface `destroy_task_unreaped_total` | **OPEN** — counter exists driver-side; harness gap. | (driver-side / harness; out of crate scope.) |

The two r25/r26 CRITICALs stay closed. R27-P1 has no new signal because the cluster failed upstream of the wake path.

---

## CRITICAL

None this round.

---

## IMPORTANT

### R28-P1 NEW IMPORTANT — `CreateGuard::drop` inline 5 s sleep blocks `detach_isolated` rollback runtime; acceptable for create-failure path, but escalates if rollback rate ever spikes

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:2302-2317` (R28-C1 inline sleep), `crates/sandbox/src/detach.rs:76-110` (`detach_isolated` shape).

**Fix shape (post `9e1f6276`):**

```rust
// nomad_ch.rs:2302-2317
if !release_delay.is_zero() {
    compio::time::sleep(release_delay).await;
}
vm_index_allocator
    .lock()
    .unwrap_or_else(|p| p.into_inner())
    .release(i);
tracing::info!(
    vm_index = i,
    reason = "create-failure-cleanup",
    sandbox_id = %sandbox_id,
    delay_ms = release_delay.as_millis() as u64,
    "sandbox/nomad-ch vm_index released (r24-A2-S3 delayed, R28-C1 inline)"
);
```

This runs INSIDE the future passed to `detach_isolated("create-rollbk", …)` (`nomad_ch.rs:2215`). `detach_isolated` (`detach.rs:84-101`) spawns a dedicated `std::thread::Builder::new().name("create-rollbk").spawn(...)`, mints a fresh `compio::runtime::Runtime`, and `block_on`s the cleanup future. With this fix, `block_on` cannot return until `sleep(5s).await` completes — i.e. the OS thread is alive for the full 5 s delay plus the Nomad purge HTTP call (~10 ms typical, 10 s timeout).

**Perf-cost breakdown per CreateGuard::drop:**

1. **Pre-fix wall (Option A from R28-C1 task brief):**
   - Nomad purge HTTP DELETE: ~10 ms typical, 10 s timeout (`http_delete_unsigned` at nomad_ch.rs:2226).
   - `rm -rf host_dir` (if `host_dir_created`): ~5-50 ms typical depending on workspace size.
   - `spawn_delayed_release` was a `compio::runtime::spawn(...).detach()` returning immediately — added ~1 µs.
   - **Total: ~15-60 ms typical, 10 s ceiling.**
   - **The bug**: the detached release task was planted on the short-lived runtime, which dropped via `Scheduler::clear` before the 5 s timer fired → vm_index leaked (compio 0.11 `runtime/mod.rs:389-399`).

2. **Post-fix wall:**
   - Same Nomad purge + rm steps (~15-60 ms).
   - **Inline `sleep(5s).await`** — the OS thread blocks for 5 s before `release()`.
   - **Total: ~5.015 - 5.060 s typical, 10 s + 5 s = 15 s ceiling.**

**Resource cost per rollback:**

| Resource | Pre-fix | Post-fix | Delta |
|---|---|---|---|
| Dedicated OS thread | ~15-60 ms alive | ~5.015-5.060 s alive | +5 s per rollback |
| Private `compio::runtime::Runtime` | ditto | ditto | +5 s |
| `Arc<Mutex<VmIndexAllocator>>` strong-ref retention | ~15-60 ms | ~5 s | +5 s |
| `vm_index` allocator slot | held the full window | held the full window (same semantics, intentional defense-in-depth) | none |

**Hot path frequency:**

`CreateGuard::drop` runs ONLY when `guard.disarm()` was NOT called on the create path. `disarm()` fires at the single success point at `nomad_ch.rs:716` (after `try_create` returns `Ok`). Per the success-disarm shape, drop-as-cleanup runs only on:

- mkfs failure (spawn_blocking branch)
- driver-stage failure (Option C Phase 2 ternary)
- Nomad submit failure
- `wait_for_alloc_running` timeout/error
- `wait_for_agent_livez` timeout
- any error inside `try_create`'s tail

T-8b-stress-r8 reported `CREATE 100% 60/60` under flag ON — i.e. zero rollbacks fired during the 60-cycle smoke. T-8b-stress-r9 was 0/400 but **failed upstream of CreateGuard entirely** (driver never loaded so `wait_for_alloc_running` returned alloc-Failed → guard disarmed? not quite; see check below). Production rate at green steady state is ~0/sec.

**Sanity check on stress-r9 RED rollback rate:** when Nomad rejects the alloc (driver unhealthy → task rejected), `wait_for_alloc_running` returns `Err`, `try_create` returns `Err`, `disarm()` is NOT called, `CreateGuard::drop` fires. At stress-r9's 400-CREATE volume against 3 workers, that's potentially **400 concurrent `create-rollbk` OS threads** each alive for 5 s. Per-controller worst-case is 400/3 ≈ 134 simultaneous OS threads. At a `THREAD_STACK_SIZE` default of 8 MiB (Linux glibc default), that's 1 GiB of committed VAS per controller during a 5 s rollback storm. **The committed RSS is much smaller** (compio runtimes are lightweight; the sleep doesn't touch the stack), but the kernel still tracks the VAS reservation.

**Risk assessment:**

- **Steady-state green path:** zero impact. CreateGuard::drop fires on failure only.
- **Stress under cluster-RED upstream:** the rollback path is the failure-amplification chain. Each CREATE that hits CreateGuard::drop now holds an OS thread + Arc<Mutex> for 5 s, vs ~50 ms pre-fix. **In a 400-CREATE storm against 36 fleet slots, the rollback storm produces ~400 OS-thread-seconds of additional thread retention** — 100× the prior load.
- **OS thread limit (`/proc/sys/kernel/threads-max`)**: GCE n1-standard default is ~30k. 400 concurrent OS threads is ~1.3% of the ceiling; non-fatal.
- **`detach_isolated` thread-spawn failure handling** (`detach.rs:102-109`): logs at error level and drops the future. Under thread-exhaustion the vm_index would leak with no release task; this is a pre-existing tradeoff and is independent of the 5 s sleep.

**Non-blocking alternative (NOT recommended for landing — but worth documenting):**

The "correct" non-blocking shape would be:
1. Issue Nomad purge inline.
2. Release vm_index AFTER an external timer fires (e.g. a long-lived `VmIndexAllocator` reaper task running on a process-lifetime compio runtime that holds a `BTreeMap<Instant, u16>` queue).
3. Or — drop `release_delay` to zero in `CreateGuard::drop` only, since the create-failure path's tap-deletion-verify window is narrower (the wrapper never finished `tuntap-add` if alloc never reached Running).

Option 3 has appeal: when alloc fails BEFORE `wait_for_agent_livez` succeeds, the driver's `start_task` never completed → the tap was never added to the host kernel → there's no eviction window to defend against. Cost: ~1 LOC + a doc-comment explaining the asymmetry. **But this is a defense-in-depth claim, and the r24-A2-S3 task brief intentionally kept the controller-side delay uniform across stop/create-rollback to avoid future-cycle reasoning errors.**

**Recommendation:** ACCEPT R28-C1 as landed. The 5 s OS-thread retention on rollback is acceptable for the steady-state-green path. If stress-r9 retry surfaces rollback-storm OS-thread pressure, consider:
1. Lowering `vm_index_release_delay_secs` to 2 s for the create-failure-cleanup branch only (introduce `vm_index_release_delay_create_failure_secs`).
2. Pre-flight: instrument `sandbox_create_rollback_active_threads` gauge (read `procfs::Process::current().task_count()` minus a baseline) so storms are observable.

**Priority:** IMPORTANT — new 5 s OS-thread-seconds per create-failure rollback. Steady-state-green path zero impact, but stress-r9-class failure amplifies linearly.

---

## MINOR

### R28-P2 NEW MINOR — `vm_index_release_delay_secs=5` narrows per-worker effective slot pool from 12 to ~7-8 under sustained-burst wake

**File:Line:** `crates/sandbox/src/config.rs:460` (config field), `:827` (default 5), `crates/sandbox/scripts/gcp-worker-startup.sh:93` (`VM_INDEX_CEIL=12`), `crates/sandbox/src/backend/nomad_ch.rs:342-385` (`spawn_delayed_release`), `:1315-1332` (stop_inner call site).

**Production config @ stress-r9:**

```
SANDBOX_NOMAD_CH_VM_INDEX_CEIL=12        # per-worker
SANDBOX_NOMAD_CH_VM_INDEX_RELEASE_DELAY_SECS=5
WORKER_COUNT=3                            # fleet total = 36 slots
```

**Math:**

- Stress-r4 smoke `WAKE 45.6 s p50` (cluster signal from r27); add stop time: `STOP fence ≤ 30 s` (`host_fence_timeout_secs=30`).
- Pre-r24-A2-S3 wake→stop cycle time: ~45.6 + 30 = ~75 s per slot end-to-end.
- Post-r24-A2-S3: +5 s on the release path = ~80 s per cycle.
- Pre-fix steady-state slot utilization at c=20 burst × 3 workers: 20 ÷ 36 = 56% (well within headroom).
- Post-fix steady-state utilization at c=20 burst × 3 workers, accounting for the extra 5 s retention: 20 × (80÷75) ÷ 36 = 59% (slight uptick).

**The real risk is restore-storm shape**, not steady-state c=20. Consider 36 concurrent CREATEs landing on 3 workers in <1 s, each hitting `try_create` and successfully reaching `host_fence_passed` at staggered times. The first 36 CREATEs consume all 36 slots; the 37th must wait for a release. With release_delay=5 s, the 37th waits an extra 5 s above the prior steady-state.

**Per-worker burst tolerance:**

```
Pre-fix:  burst ceiling = 12 slots; release at t=stop_complete.
Post-fix: burst ceiling = 12 slots; release at t=stop_complete + 5s.
```

If a worker sees 12 concurrent stops at t=0, all 12 slots release at t=5s rather than t=0. The next 12 CREATEs that need slots on this worker are delayed by 5 s relative to the prior shape.

**Defense:** the brief notes "r24-A2-S2 verify gate closes the worker-side tuntap-add window in the same release; this controller-side delay adds defense-in-depth for the rest of the per-VM state (host_dir GC, sweeper races, etc.)". The 5 s number anchors on the driver's tap-deletion-verify budget ceiling — it's intentionally aligned. Tightening below 5 s risks under-budgeting the defense-in-depth claim.

**Measurable impact under stress-r9 retry:**

- Whisker on slot-acquisition latency for the 13th+ CREATE on each worker should widen ~5 s.
- If the harness drives at c=20 against 3 workers (60 fleet-wide stagger), expect 24 CREATEs to land in the "wait for release" pool. The retry should report `wait_for_alloc_running` distributions that include a +5 s mode for these.

**No fix proposed.** The delay is the defense-in-depth feature; it's working as intended. Operator visibility on slot-pool saturation is the real ask.

**Recommendation:** add an observability counter for "CREATEs that observed `alloc` returning `IndexExhausted`" (currently the `VmIndexAllocator::alloc` `Err` path doesn't increment a metric). ~5 LOC. Defer to post-stress-greens.

**Priority:** MINOR — anchored against r24-A2-S3 defense-in-depth justification. The delay is intentional. Cluster signal on whisker widening will come from stress-r9-retry.

---

### R28-P3 NEW MINOR — `VmIndexAllocator::spawn_delayed_release` re-locks the `Arc<Mutex<VmIndexAllocator>>` per release, not coalesced

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:363-385`.

**Code shape:**

```rust
pub fn spawn_delayed_release(
    allocator: Arc<Mutex<Self>>,
    i: u16,
    delay: Duration,
    reason: &'static str,
    sandbox_id: Uuid,
) {
    compio::runtime::spawn(async move {
        if !delay.is_zero() {
            compio::time::sleep(delay).await;
        }
        allocator
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .release(i);
        tracing::info!(/* ... */);
    })
    .detach();
}
```

**Concern:** in a stop-storm (N stops in <1 s on the same worker), N detached tasks each `compio::runtime::spawn` + sleep 5 s + lock + release. The lock is held for ~1 µs per release (`BTreeSet::insert`), so contention is nil. But the spawn cost is paid N times.

**Cost components per release:**
1. `compio::runtime::spawn` allocation: ~50-100 ns.
2. `compio::time::sleep(5s).await`: ~1 timer registration + 1 timer wakeup, ~1-5 µs.
3. `Arc<Mutex<…>>::lock()`: ~50 ns (uncontended).
4. `BTreeSet::insert`: ~100 ns.
5. `tracing::info!` with 4 fields: ~1-5 µs.

**Per-stop cost: ~5-15 µs. At c=20 stop-storm: ~100-300 µs total. Immeasurable vs ~80 s wake-stop cycle.**

**Alternative shape:** a process-lifetime reaper task with a `BTreeMap<Instant, Vec<u16>>` queue + `compio::time::sleep_until` loop. Lower spawn count, but adds 1 long-lived task + structural complexity. Net: not worth it at current scale.

**Defense:** the per-stop spawn cost is dwarfed by the 5 s sleep itself + the surrounding wake/stop wall. Coalescing buys nothing measurable.

**Recommendation:** none. The current shape is correct; just noting the structural simplification is rejected at this scale.

**Priority:** MINOR — boundary-observation only.

---

### R28-P4 NEW MINOR — `CreateGuard::drop` now allocates a `Duration::from_millis(...)` per drop for the `delay_ms` tracing field

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:2313` (`delay_ms = release_delay.as_millis() as u64`).

**Code shape:** `as_millis()` returns `u128`; cast to `u64`. No heap allocation. Same for the sibling `stop_inner` site at `:1324` (which calls into `spawn_delayed_release`, which also logs `delay_ms`).

**Cost per drop:** 1 integer conversion (~1 ns). Strictly less than the prior tracing call. Immeasurable.

**This is included only to dispel the surface-level concern raised in the task brief that the "delay_ms" field shape might be expensive.** It is not.

**Priority:** MINOR — CLOSED ZERO-COST.

---

### R28-P5 NEW MINOR — `gcp-worker-startup.sh` heredoc fix at `a3cfca10` is a build-script change, not a runtime perf change

**File:Line:** `crates/sandbox/scripts/gcp-worker-startup.sh:511` (changed `<<EOF` → `<<'EOF'`).

**Cost analysis:** the heredoc-quoting fix only affects systemd-unit installation at GCE worker boot. Bash literal vs command-substituted heredoc has no runtime perf delta after the unit is written. Net: zero runtime cost, +infinity cluster-validity correctness (the prior shape silently aborted the systemd-unit write, then skipped ch-driver-install entirely).

**Priority:** MINOR — CLOSED ZERO-COST. Listed for completeness because it's the perf-relevant signal we needed to unblock stress-r9-retry.

---

### R27-P2 / R27-P3 carry — pg pool retention math; race-loser pool handshake

**Status:** UNCHANGED. r28 didn't touch `db.rs`. T-8b-stress-r9 didn't exercise pg long enough to surface new pool data (cluster ran 16 min total but CREATE-zero means controller pg activity was driver-failure-poll + admin-handler queries only). Carries forward.

**Priority:** MINOR carry.

---

## LATENT / OPEN (carry from r27)

### R16-P3 / R16-P5 — Fuse encrypt + SHA, gzip pre-AEAD

**File:Line:** `crates/sandbox/src/snapshot_handler.rs`, `crates/sandbox/src/snapshot_aead.rs` — unchanged since r26 audit.

**Status:** OPEN. No work landed. Both items are snapshot-path SHA + AEAD bandwidth optimizations; they remain TODO. **They are NOT blocking stress-r9 (which fails at CREATE, before any snapshot fires).**

**Priority:** OPEN-CARRY. Will revisit after stress-r9-retry produces a green snapshot path.

### R17-P2 — Active-set cache for sweep loops

**File:Line:** `crates/sandbox/src/sweep.rs` — unchanged.

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

### R5-P1b — SHA + BufReader path in snapshot_handler

**File:Line:** `crates/sandbox/src/snapshot_handler.rs` — unchanged.

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

### R26-M4 / R26-T1 — STOP path-split instrumentation; `destroy_task_unreaped_total` surfacing

**File:Line:** harness-side / driver-side; out of crate scope for this review.

**Status:** OPEN. No harness change.

**Priority:** OPEN-CARRY.

---

## Cross-lens consensus

- **Concurrency r28** (round-40, `docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r28.md`): identified R28-C1 — the vm_index leak that the inline-sleep fix at `9e1f6276` closed. Concurrency view validates the inline-sleep shape is correct (the dedicated OS thread + private compio runtime stays alive until `block_on` returns, which now waits for the 5 s sleep). Concurrency-correctness ✓.
- **Security r28** (round-40): R28-C1 fix doesn't touch any signed paths or auth surface. No security implication.
- **Test-coverage r29** (round-40): R28-C1 regression test `create_guard_drop_releases_vm_index_under_isolated_runtime` at `nomad_ch.rs:4623-4687` is correctly framed as a `#[test]` (NOT `#[compio::test]`) so the ambient runtime is plain `std::thread`, mirroring `detach_isolated`'s dispatch shape. Test-coverage validates the regression-pin shape.
- **Architecture r28**: Option C Phase 2 driver-staging is unchanged; both the spawn_blocking branch and the driver-staged branch coexist correctly. The R28-C1 fix only touches `CreateGuard::drop`, which is downstream of the staging branch choice.
- **API-surface r27**: BackendBuilder is unchanged; R27-API2 gating `_test_inject_sandbox` under `cfg(test_support)` is a compile-time change with zero runtime perf delta.
- **Code-quality r28** (round-39, `88cff61d`): doc-comment density on the R28-C1 fix (`nomad_ch.rs:2275-2301` carries a 26-line rustdoc explaining why the inline form is required) is high but justified — the bug is non-obvious and the comment is the operator's only signal that `stop_inner` and `CreateGuard::drop` cannot share the helper.

---

## Net assessment

**R28-C1 fix accepted.** The inline 5 s sleep inside `CreateGuard::drop` blocks the dedicated rollback OS thread for the configured `release_delay` (production 5 s) before releasing the vm_index. On the steady-state-green path this fires zero times per minute (`disarm()` runs on every successful create); on the stress-r9-class RED path where every CREATE hits rollback, the per-controller worst-case is ~134 simultaneous OS threads each alive for 5 s — non-fatal at GCE n1-standard scale (1.3% of the ~30k thread ceiling) but a 100× increase over the prior shape's ~50 ms thread-retention. **No non-blocking redesign is warranted; the rollback path is intentionally rare.**

**r24-A2-S3 5 s release-delay accepted.** Index pressure at 12-slot ceiling × 3 workers is workable at the current steady-state utilization (59% post-fix at c=20). The whisker on slot-acquisition latency for the 13th+ CREATE on each worker will widen ~5 s; cluster signal pending stress-r9-retry. The 5 s value is anchored against the driver's tap-deletion-verify budget; tightening below 5 s undermines the defense-in-depth claim.

**Two perf-affecting fixes landed; both are intentional defense-in-depth that costs steady-state-zero and adds linear-with-failure-rate retention.** No perf regressions on the green path. The stress-r9 cluster RED is upstream of either fix (heredoc bug at `gcp-worker-startup.sh:511` prevented the ch driver from loading); cluster-validated perf efficacy is pending STRESS-R9-RETRY-2.

**Backlog stays open** — R16-P3, R16-P5, R17-P2, R5-P1b all unchanged from r26-r27. None are blocking the current cluster wedge.

**R27-P1 (T5 `/version` serial RTT) stays IMPORTANT-CARRY** — no parallelize landed, no cluster signal because the wake path was never exercised in stress-r9. Re-evaluate after STRESS-R9-RETRY-2 produces a green wake distribution.

**No new perf regressions introduced by r27→r28 landings.** Both fixes are correct under their failure-mode-only invocation discipline.
