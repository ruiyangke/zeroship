# Sandbox/snapshot-restore — performance r29 review

Date: 2026-05-25 (UTC).
HEAD at audit: `e66d5efb`.
Driver HEAD pin (per `gcp-worker-startup.sh`): v20 (`116a0416`).
Controller version: v37 (`zeroship-sandbox.snapshot-v37`, `e66d5efb`).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r28.md` (HEAD `a3cfca10`).
Cluster signal: T-8b-stress-r9-retry-4 PARTIAL — `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-stress-r9-retry-4.md`. 6/400 CREATE (394 fast-fail on `vm-index allocator exhausted`), 1/6 WAKE, 6/6 SNAPSHOT, 6/6 STOP. `destroy_task_lock_held_total = 22/22` (r5-A budget exhausted on every destroy). `destroy_task_tap_stuck_total = 0` — r24-A2-S2 binding wedge VALIDATED.

Round 29 evaluates three new perf-affecting landings since r28:

1. **R29-C1 class-fix** (`62b083e1`) — replaces the fire-and-forget `VmIndexAllocator::spawn_delayed_release` helper with the inline-await `release_vm_index_after` + typed-Task escape hatch `spawn_delayed_release_in_worker`. The pre-r29 helper detached the timer task onto the CURRENT runtime; under `detach_isolated`'s short-lived runtime (snap-teardown, create-rollbk paths) the task got discarded by `Scheduler::clear` → vm_index leaked. The fix routes `stop_inner` and `CreateGuard::drop` through the inline-await variant. **Wall-time cost: `stop_inner` HTTP wall now includes `vm_index_release_delay_secs` (5 s prod default). Modelled below in R29-P1.**
2. **R28-I1 + R28-I2 parallelize** (`d00f12dd`) — `wake_machine::run` now `futures::join!`s `verify_agent_version_post_restore` + `clock_resync_post_restore_typed` instead of serial. Failure-path latency drops from `t5 + clock_resync` (≈ 20 s) to `max(t5, clock_resync)` (≈ 10 s). Plus a structured `transport_error: bool` boolean enabling the half-dead-agent detector. **Net wake-path latency reduction confirmed; see R29-I1 below.**
3. **Driver v20** (`116a0416`) — 16 enumerated restore stages + per-stage counter labels. Observability, no runtime perf delta on the controller. Out of crate scope for this review (driver-side); flagged for completeness.

## Summary

**11 findings, 0 CRITICAL, 1 IMPORTANT (1 NEW), 6 MINOR (3 NEW, 3 CARRY), 4 LATENT/OPEN (carry).**

One new IMPORTANT:

- **R29-P1 NEW IMPORTANT** — `stop_inner` HTTP wall +5 s on the synchronous stop path (`handlers.rs:667` and `registry.rs:868`). `state.backend.stop(id).await` is awaited inline before the HTTP 200 response. With `vm_index_release_delay_secs=5` (prod default), the user-visible stop latency now adds 5 s vs pre-r29-A2. Steady-state acceptable (stops are a write-path operation and aren't latency-critical), but the `start_idle_gc` loop at `registry.rs:836-876` serialises stops one-by-one — N expired sandboxes pay N×5 s of release delay.

Two new MINORs:

- **R29-P2 NEW MINOR** — parallel T5 + clock_resync (`d00f12dd`) doubles the peak compio blocking-pool worker demand for ~0-10 s of the wake path. Both probes call `compio::runtime::spawn_blocking` and run concurrently under `futures::join!`. At c=20 wake-storm × 3 workers the blocking pool can spike to 2× the prior steady-state. Compio's blocking-pool default is large (workstation-class), so this is observation-only.
- **R29-P3 NEW MINOR** — `clock_resync_post_restore_typed` re-derives `transport_error` by `String::starts_with("/_clock_resync transport:")` on the error message (`restore_handler.rs:3091-3092`). The boolean is now load-bearing for the half-dead-agent detector (R28-I2), yet the source of truth is still a stringly-typed prefix-match on a `format!`ed wrapper. One refactor in the underlying function (`format!("/_clock_resync transport: {e}")` → `format!("/_clock_resync: transport error: {e}")`) silently inverts the detector. Defense: TBD; the test at `clock_resync_typed_surfaces_transport_error_on_closed_port` pins the prefix.

Carry table (one carry promoted from R28):

- **R28-P2 carry IMPORTANT → MINOR DEMOTED** — the 5-second `vm_index_release_delay_secs` × 12-ceil × 3-workers shape is now cluster-validated (stress-r9-retry-4: 394/400 CREATE fast-fail on allocator exhaustion at c=20). R28-P2's predicted whisker-widening did NOT materialise (the c=20 burst saturated the allocator before any release-delay whisker could be measured). Demoted because the wedge is now a harness/ceiling mismatch (cluster review §3), not a perf regression on the current shape.

One R28 IMPORTANT closed:

- **R28-P1 CLOSED** — the `CreateGuard::drop` inline-sleep at `9e1f6276` was superseded by the `release_vm_index_after` helper at `62b083e1`. Same wall-time semantics, cleaner shape, single source of truth for the release-delay log line. The R28-P1 risk assessment (5 s OS-thread retention per rollback) still holds; just no longer a code-shape concern.

One R27 IMPORTANT closed:

- **R27-P1 CLOSED** — T5 + clock_resync now parallel via `futures::join!` (`wake_machine.rs:518-530`). The 10 s reduction in wake-failure-path latency (`d00f12dd`) is exactly the fix R27-P1 asked for.

## Carry table

| Finding | Status @ r29 | Evidence |
|---|---|---|
| **R28-P1** CreateGuard::drop inline 5 s sleep | **CLOSED** at `62b083e1` — now routes through `release_vm_index_after`. Same semantics, cleaner shape. | `crates/sandbox/src/backend/nomad_ch.rs:2342-2380` (CreateGuard::drop), `:345-394` (helper). |
| **R28-P2** vm_index_release_delay narrows slot pool | **CARRY MINOR (demoted from IMPORTANT)** — cluster signal is allocator-exhaustion at c=20, not release-delay whisker. | `crates/sandbox/scripts/gcp-worker-startup.sh:93` (ceil=12), cluster review §3. |
| **R27-P1** T5 /version serial with clock_resync | **CLOSED** at `d00f12dd`. | `crates/sandbox/src/wake_machine.rs:485-548` — now `futures::join!`. |
| **R26-C1** thread-local pg pool | **CLOSED** (carry from r28). | `crates/sandbox/src/db.rs:617-642` — unchanged. |
| **R26-I2** `try_create` spawn_blocking | **CLOSED** (carry from r28). | `crates/sandbox/src/backend/nomad_ch.rs:842-866` — unchanged. |
| **R16-P3** Fuse encrypt + SHA | **OPEN-CARRY** — no work landed. | snapshot_handler/aead untouched. |
| **R16-P5** gzip pre-AEAD | **OPEN-CARRY** — no work landed. | snapshot_aead.rs untouched. |
| **R17-P2** Active-set cache | **OPEN-CARRY** — no work landed. | sweep.rs untouched. |
| **R5-P1b** SHA + BufReader path | **OPEN-CARRY** — no work landed. | snapshot_handler.rs untouched. |
| **R26-M4** STOP path-split instrumentation | **OPEN-CARRY** — harness gap. | (driver-side / harness; out of crate scope.) |
| **R26-T1** Surface `destroy_task_unreaped_total` | **OPEN-CARRY** — driver-side. | (driver-side / harness; out of crate scope.) |

---

## CRITICAL

None this round.

---

## IMPORTANT

### R29-P1 NEW IMPORTANT — `release_vm_index_after(...).await` adds 5 s to `stop_inner`'s HTTP wall on the synchronous stop path; `start_idle_gc` serialises N×5 s

**File:Line:**
- Helper: `crates/sandbox/src/backend/nomad_ch.rs:345-394` (`release_vm_index_after`).
- Call site (stop): `crates/sandbox/src/backend/nomad_ch.rs:1393-1402` (in `stop_inner`).
- HTTP wall (synchronous): `crates/sandbox/src/handlers.rs:667` (`if let Err(e) = state.backend.stop(id).await { ... }` — awaited before HTTP 200).
- HTTP wall (synchronous): `crates/sandbox/src/backend/mod.rs:348-356` (the `Backend::stop` indirection — direct passthrough to `nomad_ch::stop_inner`).
- Serial GC loop: `crates/sandbox/src/registry.rs:854-873` (one stop awaited per expired sandbox).
- Async/detached site (NOT on HTTP wall): `crates/sandbox/src/admin_handlers.rs:1486-1506` (snap-teardown is `detach_isolated`-wrapped — HTTP response goes out before stop completes).

**Pre/post wall on `stop_inner`:**

```text
Pre-r29-A2 (commit a3cfca10):
  Nomad purge HTTP DELETE: ~10 ms typical, 10 s timeout
  host_fence agent-silent wait: ~100 ms-5 s typical, 30 s timeout
  spawn_delayed_release (fire-and-forget): ~1 µs (helper returns, .detach())
  rm -rf host_dir (if remove_host_dir): ~5-50 ms
  --- HTTP 200 returns here ---
  Background: timer task fires at +5 s, calls .release()
  Total HTTP wall: 15 ms - 35 s (dominated by host_fence + Nomad purge)

Post-r29-A2 (commit 62b083e1):
  Nomad purge HTTP DELETE: ~10 ms typical, 10 s timeout
  host_fence agent-silent wait: ~100 ms-5 s typical, 30 s timeout
  release_vm_index_after.await: sleep(5 s) + lock + release + log
  rm -rf host_dir (if remove_host_dir): ~5-50 ms
  --- HTTP 200 returns here ---
  Total HTTP wall: 5.015 s - 40 s (5 s added in the median)
```

The commit message claims "negligible vs the host_fence (up to 30 s) and Nomad purge waits already on this path". This is true at the **30 s tail** but understates the **p50**: the host_fence's median is much shorter than 30 s when the agent is healthy and answering livez. From the cluster signals at smoke-r4: `STOP fence ≤ 30 s` ceiling but typical clearance is in the 100 ms-1 s range when the agent is shutting down normally. The 5 s release delay is **additive on top** of that.

**Modelling:**

Assume agent-healthy stop:
- Pre-r29: ~150 ms-1.5 s (Nomad purge + brief fence + log)
- Post-r29: ~5.15 s-6.5 s (above + 5 s release-delay sleep)

Assume agent-unresponsive stop (half-dead):
- Pre-r29: ~10-30 s (long fence timeout)
- Post-r29: ~15-35 s (above + 5 s release-delay sleep)

So p50 HTTP wall on the stop path moved from sub-second to ~5 s. The commit message's "negligible" framing is true on the **failure tail** but missed the **green-path p50 shift**.

**`start_idle_gc` loop amplification:**

```rust
// registry.rs:854-873
for id in to_kill {
    // ...
    if let Err(e) = state.backend.stop(id).await {  // ← serial .await
        tracing::warn!(...);
    }
    // ...
}
```

The idle-gc loop iterates expired sandboxes serially. At post-r29-A2 the per-iteration cost is +5 s vs pre-r29. If a controller has N expired sandboxes ready at the 60 s GC tick:

- Pre-r29: N × ~1 s = N seconds of GC loop wall.
- Post-r29: N × ~6 s = 6N seconds of GC loop wall.

At N=12 (a single worker's full ceiling): 12 s → 72 s. The GC loop now overruns its own 60 s interval, so the next tick starts before the prior batch completes — backlog grows. **This is a steady-state regression: not fatal, but the GC throughput drops 6×.**

**Fix candidates (none landed):**

1. **Parallelise the GC loop's stops** — `futures::join_all` over the `to_kill` set. Each stop runs concurrently on the OS-thread-private compio runtime; the 5 s delay is wallclock-shared across N stops. Cost: ~1 LOC change at `registry.rs:854-874` plus a `Vec<_>` collect. Risk: N concurrent Nomad-purge HTTP calls + N concurrent host_fence GETs against agents fan out over the cluster — typically fine (each lands on a different agent), but a single misbehaving controller pool node could see N concurrent HTTP fan-outs. Mitigation: bounded-parallel via `FuturesUnordered` + a small concurrency cap (4-8).
2. **Detach the release-delay from the HTTP wall on the stop handler** — the snap-teardown path already uses `detach_isolated` to take the release-delay off the HTTP wall (the admin handler responds 200 before teardown completes). The `handlers.rs:667` stop site could mirror this pattern: respond 200 after fence clears, run the release-delay sleep in a detached `detach_isolated("stop-release", …)` future. Cost: ~15 LOC. Risk: a wake racing the detached release would still see `reserve(vm_index)` reject with "already reserved" (the in-memory allocator is `Arc<Mutex<…>>`-shared); the contract is preserved.
3. **Lower `vm_index_release_delay_secs` on the GC/stop paths only** — introduce a separate `vm_index_release_delay_gc_secs` and `vm_index_release_delay_stop_secs` knob (defaults: 5 s for GC, 5 s for stop, but allow tuning). Net: same wall as today, but adjustability per-path. Cost: ~30 LOC + config plumbing. Risk: muddies the defense-in-depth invariant (release-delay aligns with driver tap-deletion-verify budget).

**Recommendation:** ACCEPT the R29-C1 class-fix as a correctness win. **Defer R29-P1 fix candidate 1 (parallelise GC) to the post-stress-greens window** — the stress-r9-retry-4 cluster failure is upstream of this (CREATE allocator exhaustion at c=20 vs ceil=12), so the GC throughput regression is not on the current critical path. **Once steady-state is green, parallelise the GC loop** — 6× throughput recovery for ~5 LOC.

**Priority:** IMPORTANT — green-path p50 stop latency 1 s → 5 s (5× increase). GC loop throughput drops 6× at full-ceiling expiry. Not a current cluster-wedge factor, but a measurable regression once stress-r9-retry-N goes green.

---

## MINOR

### R29-P2 NEW MINOR — parallel T5 + clock_resync probes (`d00f12dd`) double the compio blocking-pool peak demand for 0-10 s of the wake path

**File:Line:** `crates/sandbox/src/wake_machine.rs:518-530` (parallel join), `crates/sandbox/src/restore_handler.rs:2967-3030` (clock_resync `spawn_blocking`), `:3256-3289` (T5 `spawn_blocking`).

**Code shape:**

```rust
// wake_machine.rs:518-530
let t5_future = crate::restore_handler::verify_agent_version_post_restore(/* … */);
let clock_resync_future = crate::restore_handler::clock_resync_post_restore_typed(/* … */);
let (t5_outcome, clock_resync_outcome) =
    futures::join!(t5_future, clock_resync_future);
```

Both futures are thin wrappers that immediately `compio::runtime::spawn_blocking` to issue a `ureq` call (10 s timeout each). Under `futures::join!` they're polled concurrently on the same caller task, so both `spawn_blocking` calls fire near-simultaneously → 2 blocking-pool worker threads consumed per wake for the duration of `max(t5_wall, clock_resync_wall)`.

**Concern.** Pre-r28-I1: each wake consumed 1 blocking-pool worker for ~`t5_wall + clock_resync_wall` (serial). Post-r28-I1: each wake consumes 2 blocking-pool workers for ~`max(t5_wall, clock_resync_wall)` — same total CPU-seconds, but **doubled instantaneous worker demand**.

**Math at c=20 wake-storm × 3 workers (stress-r9-retry-N target):**

- Pre-fix peak blocking-pool demand: 20 workers (one per concurrent wake).
- Post-fix peak: 40 workers (two per concurrent wake during the parallel join window).

Compio's default blocking pool is the larger of `RAYON_NUM_THREADS` / CPU-count or 32 (`compio-runtime` 0.7 defaults — needs verification per crate; this is a structural concern, not a measured one). On n2-standard-32 controllers (32 vCPU) the default pool is likely 32-256 threads — 40 concurrent blocking calls fits comfortably. On n2-standard-4 servers (the harness's controller shape per stress-r9-retry-4 cluster review) the default is ~32 threads — 40 wakes would saturate the pool, queuing the 41st `spawn_blocking`. **At c=20 this is fine; at c=30+ wake-storm against a 4-vCPU controller, the pool becomes a queue.**

**Defense:** the wake throughput is bounded by `wake_concurrency_limit` upstream (`crates/sandbox/src/wake_machine.rs` — verified). At the harness-target c=20, the doubled demand is non-fatal. The 10 s reduction in wake-failure-path latency is real and measurable; this is a fair tradeoff.

**Recommendation:** none. Observation-only at current cluster scale. **If a future stress-r10+ goes to c=40+ on a 4-vCPU controller, surface `compio_blocking_pool_queue_depth` (if compio exposes it) before raising the wake concurrency limit.**

**Priority:** MINOR — instantaneous pool demand doubled; tail-latency reduction is the dominant signal.

---

### R29-P3 NEW MINOR — `clock_resync_post_restore_typed`'s `transport_error` bool derives via `String::starts_with("/_clock_resync transport:")` — load-bearing string prefix-match

**File:Line:** `crates/sandbox/src/restore_handler.rs:3083-3098`.

**Code shape:**

```rust
pub(crate) async fn clock_resync_post_restore_typed(/* … */) -> ClockResyncOutcome {
    match clock_resync_post_restore(agent_url, sandbox_id, signing_key_bytes).await {
        Ok(()) => ClockResyncOutcome::Ok,
        Err(message) => {
            let transport_error =
                message.starts_with("/_clock_resync transport:");
            ClockResyncOutcome::Err { transport_error, message }
        }
    }
}
```

The underlying `clock_resync_post_restore` returns `Err(format!("/_clock_resync transport: {e}"))` on the ureq transport-error branch (line 3028). The typed wrapper reads that prefix.

**Concern.** R28-I2's commit message explicitly calls out:

> The two probes' transport-error signals were only recoverable via string-matching the diagnostic message — fragile and not a contract.
>
> Surfaced as a STRUCTURED boolean signal on the typed outcomes.

The intent was to MOVE OFF stringly-typed signalling. But the wrapper *still* does a `starts_with` prefix-match — it just confines the string-matching to a single internal location instead of leaking it to the caller. The structured boolean is now load-bearing for the half-dead-agent detector (`wake_machine.rs:547-574`), yet a single edit to `clock_resync_post_restore`'s error-format string (e.g., `format!("/_clock_resync transport: {e}")` → `format!("/_clock_resync: transport error: {e}")`) **silently inverts the detector to always-false** — both probes would return `transport_error=false` and the half-dead-agent path would never fire.

**Defense:**
- The test `clock_resync_typed_surfaces_transport_error_on_closed_port` (`restore_handler.rs`, search `clock_resync_typed_surfaces_transport_error_on_closed_port`) explicitly hits a closed port and asserts `transport_error=true`. Any refactor of the underlying function's error-format string MUST go through this test.
- The T5 sibling (`verify_agent_version_post_restore`) takes a cleaner shape — it returns `VersionCheckOutcome::Skipped { reason: "transport_error", transport_error: true }` from the `Err(transport: …)` branch directly (`restore_handler.rs:3300-3304`). No prefix-match needed because the typed enum is constructed at the source of the failure.

**Fix shape:**

Refactor `clock_resync_post_restore` to **return the typed outcome at the failure site** rather than wrapping a `Result<(), String>` and re-typing in the wrapper:

```rust
// Direct construction at the failure site (illustrative; cost ~30 LOC):
Err(e) => return ClockResyncOutcome::Err {
    transport_error: true,
    message: format!("/_clock_resync transport: {e}"),
},
```

This matches T5's shape and eliminates the prefix-match contract. Risk: `clock_resync_post_restore` has a `Result<(), String>` return that other call sites depend on (`restore_handler.rs:1149`). The cleanest path is to make `clock_resync_post_restore_typed` the primary and have `clock_resync_post_restore` convert `ClockResyncOutcome::Ok → Ok(())` / `::Err → Err(message)` for the sync site.

**Recommendation:** TBD. Not a current cluster-wedge factor. The test pins the contract; the next-cycle refactor can move the structured construction to the failure site. ~30 LOC + 1 test rename.

**Priority:** MINOR — typed boolean is technically load-bearing on a string prefix. Test pins the contract. Future-cycle cleanup.

---

### R29-P4 NEW MINOR — `spawn_delayed_release_in_worker` is `#[allow(dead_code)]` typed escape hatch; no production caller; rustdoc-only justification

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:395-440` (`spawn_delayed_release_in_worker`).

**Code shape:**

```rust
#[allow(dead_code)] // typed escape hatch — see rustdoc
pub fn spawn_delayed_release_in_worker(
    allocator: Arc<Mutex<Self>>,
    i: u16,
    delay: Duration,
    reason: &'static str,
    sandbox_id: Uuid,
) -> compio::runtime::Task<
    Result<(), Box<dyn std::any::Any + Send>>,
> {
    compio::runtime::spawn(Self::release_vm_index_after(/* … */))
}
```

**Observation.** The function is marked `#[allow(dead_code)]` and has no production caller — only the test `spawn_delayed_release_in_worker_returns_joinable_task` at `nomad_ch.rs:4831`. The justification in rustdoc:

> No current call site in this crate uses this helper; it exists as the type-safe escape hatch for any future background-task path that needs fire-and-forget delayed release on a long-lived runtime without blocking the caller.

This is a YAGNI flag. Future callers either:
- Use `release_vm_index_after(...).await` (the inline-await primary), which is the safe default everywhere.
- Use this helper + `.detach()` and need to PROVE the current runtime is long-lived. The rustdoc says "Long-lived ntex / snap-idle-gc / sweep loop callers can safely call `.detach()`" but none currently do.

The architecture-r29-A2 motivation was "force the caller to confront the runtime-lifetime question via the return type". That's a real value-add for the documentation, but the unused function carries:

- Compile-time cost (negligible).
- `#[allow(dead_code)]` lint suppression (technical debt marker).
- Test maintenance burden (one test pinning the typed-return shape).

**Defense:** keeping the typed escape hatch documents the safe-detach shape for future callers. If the alternative is "write detach + your own timer inline", the unused helper is the better doc. The `#[allow(dead_code)]` is intentional (per architecture-r29-A2's bid).

**Recommendation:** none. If a future audit ever finds 0 callers across 3+ rounds, retire the helper. For now, the cross-lens consensus (architecture-r29) accepted it as the typed-escape-hatch contract.

**Priority:** MINOR — YAGNI flag. Closed-acceptable per architecture-r29-A2.

---

### R29-P5 NEW MINOR — `release_vm_index_after` retains 3-byte `&'static str` reason + Uuid sandbox_id per call; +5 µs tracing overhead on every release

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:374-394` (the helper body).

**Code shape:**

```rust
pub async fn release_vm_index_after(
    allocator: Arc<Mutex<Self>>,
    i: u16,
    delay: Duration,
    reason: &'static str,
    sandbox_id: Uuid,
) {
    if !delay.is_zero() {
        compio::time::sleep(delay).await;
    }
    allocator
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .release(i);
    tracing::info!(
        vm_index = i,
        reason = %reason,
        sandbox_id = %sandbox_id,
        delay_ms = delay.as_millis() as u64,
        "sandbox/nomad-ch vm_index released (r24-A2-S3 delayed)"
    );
}
```

**Cost per release:**
- `compio::time::sleep(5s)`: 1 timer registration + 1 wakeup ~1-5 µs.
- `Arc<Mutex<…>>::lock()`: ~50 ns uncontended.
- `BTreeSet::insert`: ~100 ns.
- `tracing::info!` with 4 fields + Display formatting of Uuid: ~3-7 µs.

**Per-release total: ~5-15 µs of CPU.** Dwarfed by the 5 s sleep. Immeasurable in steady state. Per architectural shape this is the same as R28-P3 (the prior `spawn_delayed_release` had the same logging cost). Mentioned here only to dispel the surface-level concern about the helper's overhead.

**Priority:** MINOR — CLOSED ZERO-IMPACT.

---

### R29-P6 NEW MINOR — `start_idle_gc` loop's serial `state.backend.stop(id).await` is the canonical N×5 s amplification site for R29-P1

**File:Line:** `crates/sandbox/src/registry.rs:854-873`.

**Code shape:**

```rust
for id in to_kill {
    if let Some(info) = state.sandboxes.get(&id) { /* log */ }
    if let Err(e) = state.backend.stop(id).await {  // ← serial 5 s+ per iter
        tracing::warn!(/* … */);
    }
    let _ = std::panic::catch_unwind(/* … */);
}
```

**This is the GC-throughput regression flagged in R29-P1.** Called out as its own MINOR for line-level visibility — the fix landing site is `registry.rs:854-874` (parallelise) or `handlers.rs:667` (mirror admin's `detach_isolated`).

**Cost per GC tick at N=12 expired sandboxes:** pre-r29-A2 ~12 s; post-r29-A2 ~72 s. The next tick starts before the prior completes when interval=60 s. GC backlog grows linearly with sustained expiry rate.

**Recommendation:** parallelise via `FuturesUnordered::collect` with bounded concurrency (cap=4-8). ~10 LOC.

**Priority:** MINOR — single-file fix, deferred to post-stress-greens window per R29-P1.

---

### R27-P2 / R27-P3 carry — pg pool retention math; race-loser pool handshake

**Status:** UNCHANGED. r29 didn't touch `db.rs`. T-8b-stress-r9-retry-4 ran 192 s and produced 6 successful CREATEs + 6 SNAPSHOTs — too few to surface pool data. Carries forward.

**Priority:** MINOR carry.

---

## LATENT / OPEN (carry from r28)

### R16-P3 / R16-P5 — Fuse encrypt + SHA, gzip pre-AEAD

**File:Line:** `crates/sandbox/src/snapshot_handler.rs`, `crates/sandbox/src/snapshot_aead.rs` — unchanged since r26.

**Status:** OPEN. Snapshot-path bandwidth optimisations. **Stress-r9-retry-4 produced 6/6 SNAPSHOTs at p50=25.3 s, p95/p99/max=44.5 s — so snapshot path IS now reachable.** Once stress goes green at higher cycle counts, R16-P3/P5 are the next bandwidth wins.

**Priority:** OPEN-CARRY → revisit after stress-r9-retry-N goes green at c=20 × ≥60 cycles.

### R17-P2 — Active-set cache for sweep loops

**File:Line:** `crates/sandbox/src/sweep.rs` — unchanged.

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

### R5-P1b — SHA + BufReader path in snapshot_handler

**File:Line:** `crates/sandbox/src/snapshot_handler.rs` — unchanged.

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

### R26-M4 / R26-T1 — STOP path-split instrumentation; `destroy_task_unreaped_total` surfacing

**File:Line:** harness-side / driver-side; out of crate scope.

**Status:** OPEN. R26-T1 mostly closed — stress-r9-retry-4's cluster review captures the v20 counters (`destroy_task_unreaped_total = 0`, `destroy_task_lock_held_total = 22`, etc.) directly from `/var/lib/zsbx/driver-metrics.prom`. STOP path-split instrumentation still gap.

**Priority:** OPEN-CARRY.

---

## Cluster-side perf observations from stress-r9-retry-4

(Out of crate scope, but perf-relevant context for the next cycle.)

### vm-index allocator exhaustion: harness/ceiling mismatch, not a perf wedge

```
{"error":"backend_create_failed","message":"backend.create: vm-index allocator exhausted (floor=1, ceil=12)"}
```

394/400 CREATE failures fast-failed inside `VmIndexAllocator::alloc` at `nomad_ch.rs:323-337`. This is a STRUCTURAL mismatch:

- Harness c=20 concurrent CREATEs.
- Fleet capacity 12 ceil × 3 workers = 36 slots.
- Release delay 5 s per stop.

At c=20 the harness keeps 20 CREATEs in-flight; the allocator hands out 36 across all 3 workers' shards but each worker can only host 12. Routing isn't load-aware (CHWBL hashes by sandbox_id), so the harness's burst lands ~6-7 CREATEs per worker shard randomly — saturating one shard well before the others. The 394 fast-failures hit the over-allocated shard.

**This is a harness/config issue, not a runtime perf regression.** Three correct fixes:

1. **Harness side**: reduce concurrency to c=12 (matches per-worker ceiling) or c=36 (matches fleet ceiling, accepting CHWBL skew).
2. **Config side**: bump `vm_index_ceil` per worker (12 → 32?). The default is conservative; production tier sizing may want higher.
3. **Application side**: CREATE-side retry on `allocator exhausted` with backoff + jitter. The current shape fast-fails immediately, which is the right contract — but the harness should retry.

**No code change recommended this cycle.** The R28-P2 prediction that the release-delay would widen the slot-acquisition whisker is now ground-truth-falsified: the wedge is upstream (allocator-exhaustion before any release-delay matters).

### `destroy_task_lock_held_total = 22/22`: 5 s OFD-probe budget is **not enough** under sustained CH-exit load

```
nomad_driver_ch_destroy_task_lock_held_total 22  (out of 22 destroys)
```

Every single DestroyTask exhausted its r5-A OFD-lock probe budget. The driver-side r5-A probe times out at 5 s waiting for the kernel's `__fput` to release the rootfs.img fd lock. The cluster review's interpretation:

> r5-A bounds the wedge but doesn't solve it. The kernel's delayed __fput is taking longer than the budget.

**Perf-angle assessment of the 5 s budget:**

- Linux's `__fput` is queued onto a kernel workqueue (`task_work_run` → `__fput_sync` / `delayed_fput`). Under concurrent CH process exits, the workqueue saturates → fd-close latency grows.
- 5 s was sized against single-process CH exits. Under concurrent destroys (stress-r9-retry-4 ran 22 destroys in 192 s = ~8.7 s mean inter-destroy; bursts much tighter), the workqueue backlog blows past 5 s on every destroy.

**Correct path:**

Either (a) raise the budget (5 s → 30 s? matches `host_fence_timeout_secs`) at the cost of slower destroy reuse, or (b) fix the root-cause by understanding the workqueue saturation (likely needs `sysctl kernel.task_delay_acct=1` + `iotop -b -o -d 1` during a stress run to characterise). Driver-side fix; out of crate scope for this review.

**Cross-reference for next cycle:** the controller-side `vm_index_release_delay_secs=5` was anchored against the driver's tap-deletion-verify budget (per R28-P2 / r24-A2-S3). If the driver-side r5-A budget moves to 30 s, the controller-side release delay should follow — see config knob `crates/sandbox/src/config.rs:460,827` and the comment chain at `nomad_ch.rs:1372-1402`.

---

## Cross-lens consensus

- **Concurrency r29** (round-42, `docs/reviews/sandbox-snapshot-restore-concurrency-2026-05-25-r29.md`): R29-C1 closed (the snap-teardown vm_index leak shape). Concurrency view validates `release_vm_index_after` as the safe-default everywhere; no concurrency regression introduced.
- **Architecture r29** (round-42): R29-A2 class-fix accepted. The typed `Task<...>` return on `spawn_delayed_release_in_worker` is the contract that forces future callers to confront the runtime-lifetime question. R29-P4's YAGNI flag is the price.
- **Security r29** (round-42): R29-C1 fix touches no signed paths; no security implication. The `transport_error` boolean fingerprint is a new structured signal but reads off the same agent HTTP surface (no new attack surface).
- **Test-coverage r29** (round-42, `de7465ac`): `release_vm_index_after_survives_short_lived_runtime` test pins the R29-C1 regression contract; the inline-await form is exercised under `detach_isolated`'s private compio runtime. Coverage adequate.
- **Code-quality r29** (round-42): doc-comment density on the helper (`nomad_ch.rs:345-440`) is high (~60 lines of rustdoc + comments for ~50 lines of code) but justified — the bug class is non-obvious and the comments are the operator's signal that the inline-await is load-bearing.
- **API-surface r28**: `release_vm_index_after` is `pub` and `spawn_delayed_release_in_worker` is `pub` + `#[allow(dead_code)]`. The typed Task return is the type-safety win; the `dead_code` mark is the YAGNI signal. API surface accepts both.

---

## Net assessment

**Three landings this round; two are net wins, one is a measurable green-path regression with a clear fix path.**

**R29-C1 class-fix (62b083e1) — net win, with regression.** The vm_index leak under `detach_isolated`'s short-lived runtime is closed via `release_vm_index_after.await`. Correctness ✓. **The trade is a green-path p50 HTTP stop wall of 1 s → 5 s, and a 6× GC-loop throughput regression under sustained expiry** — see R29-P1 + R29-P6. Both are addressable post-stress-greens via the `start_idle_gc` parallelisation fix (~10 LOC).

**R28-I1 + R28-I2 parallelize (d00f12dd) — net win.** Wake-failure-path latency `t5 + clock_resync` → `max(…)` (≈ 20 s → 10 s). The structured `transport_error: bool` enables the half-dead-agent detector. R27-P1 IMPORTANT closed. **Minor caveat (R29-P3):** the typed `transport_error` is internally derived via `String::starts_with` prefix-match — the test pins the contract, but a future refactor of the underlying error-format string would silently invert the detector. Defer the cleanup.

**Driver v20 (116a0416) — observability only, no controller perf delta.**

**Two R28 IMPORTANTs closed (R28-P1 superseded by class-fix; R27-P1 closed by parallelize). One new R29 IMPORTANT (R29-P1).** Net IMPORTANT count: unchanged. Net direction: positive (the new R29-P1 has a clear fix path; the closed R28-P1/R27-P1 are structural improvements that landed cleanly).

**Cluster signal:** stress-r9-retry-4 PARTIAL is upstream of both R29 landings. The 394/400 CREATE allocator-exhaustion failures are a c=20-vs-ceil=12 harness/config mismatch, not a runtime perf regression. The 22/22 `destroy_task_lock_held_total` is a driver-side r5-A budget issue (5 s budget vs kernel `delayed __fput` under sustained concurrent CH exits taking longer than 5 s). Both are next-cycle items; neither is a perf-r29 finding.

**Backlog stays open** — R16-P3, R16-P5, R17-P2, R5-P1b all unchanged from r28. With stress-r9-retry-4 producing 6/6 snapshots (p50=25.3 s, p95=44.5 s), the snapshot-path bandwidth wins (R16-P3 Fuse encrypt + SHA; R16-P5 gzip pre-AEAD) become the next-priority bucket once steady-state is green.

**No new CRITICALs, no security or correctness regressions. The class-fix shape is sound; the regression is in p50 HTTP wall + GC throughput, addressable with a single follow-up commit.**
