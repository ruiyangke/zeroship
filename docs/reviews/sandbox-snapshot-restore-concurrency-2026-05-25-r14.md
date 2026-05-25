# Sandbox/snapshot-restore — concurrency r14 review

Date: 2026-05-25 (UTC)
HEAD at audit: `460c8fd1` (per worktree); prompt cites `d673e043` (still on branch). Both audited; C-6 fix (`91ce9be5`) sits between them and is included in scope.
Round 14 of N. Read-only. Branch `feat/sandbox-snapshot-restore`.

## Summary

- 4 NEW findings (1 critical-sibling, 1 important-sibling, 1 important diagnostic-gap, 1 verification), 1 carry-forward escalation.
- **C-6 root cause now nailed**: detached `teardown_source_for_snapshot` on the SAME ntex-worker compio runtime starved the wake handler's `reserve_vm_index_with_retry` `compio::time::sleep` continuations. The `91ce9be5` fix (OS-thread + dedicated compio runtime) is structurally correct but covers ONLY the admin/snapshot path; the same shape persists in two sibling sites (`sweep.rs:563` idle-eviction, `registry.rs:829` idle GC). File as **[R14-C1]** (sibling C-6 in idle-eviction) and **[R14-I2]** (sibling C-6 in registry idle GC).
- **`do_restore_inner` await count: 8** (was 7 through r10-r13). C-4 added `reserve_vm_index_with_retry(...).await` at line 558. The new await is itself an N-iteration `sleep(...).await` loop body — the inner loop drives 1..=60 awaits per wake under contention. Latency-impact is in perf-r14; the cancel-window count delta is the new headline. File as **[R14-V1]**.
- **C-6 diagnostic-gap audit**: the 14 phase-boundary log sites added at `8e7f0b53` did exactly the job they were designed for — localized the wedge to `pre_reserve_vm_index`. Phase tracing is INSUFFICIENT to identify WHICH detached task starved the runtime; that required code-reading. File as **[R14-D1]** (diagnostic completeness verification).
- **C-4 retry budget interaction with starvation**: prompt's hypothesis was wrong. C-4's budget of 60×2 s = 120 s isn't the issue — the runtime starvation prevented the FIRST sleep from yielding, so attempts 2..=60 never began. Budget exhaustion warn never fired because the loop never advanced past attempt 1. File as **[R14-I1]** (concurrency-correctness clarification, not a perf concern).
- All r13 carry-forwards remain open. **R4-A2 LeasedVmSlot RAII** is now **11+ cycles open** — R14-C1 sibling and R10-C1's R11-C1 carry-forward both would dissolve under a proper RAII shape, but neither dissolves under the C-6 fix in isolation.
- **R13-Q1 / R13-C1 ENV_LOCK race**: CLOSED at `c5b9cb9d` (per deferred file). Verified by Grep — `R12_I1_ENV_LOCK` gone; both modules use the unified `crate::backend::nomad_ch::test_env_lock::TASK_DRIVER_ENV_LOCK`. Carry retired.

## C-6 root cause — exhaustive analysis

### The runtime starvation shape

```
ntex worker thread N (single-threaded compio runtime per ntex worker)
├── ntex http accept loop
├── handler future A: admin/snapshot handler (returns 200 immediately after spawn().detach())
│     └── compio::runtime::spawn(async { teardown_source_for_snapshot.await }).detach()
│             ↓  (lives on SAME runtime as A)
├── detached task T:  stop_inner
│     1. http_signed_async("/shutdown")
│        └── compio::runtime::spawn_blocking(|| ureq.delete(60s timeout))
│            ── BLOCKING POOL: separate threads, NOT runtime-residency
│        ── BUT the OUTER future T is parked on this spawn_blocking's join,
│           re-polled when blocking completes (60s later)
│     2. stop_nomad_job (10s)
│     3. wait_for_job_gone (30s polled, each iter via spawn_blocking + compio::time::sleep)
│     4. wait_for_agent_silent (120s polled, each iter via spawn_blocking + compio::time::sleep)
│
├── handler future B: admin/wake (queued immediately after A's 200)
│     └── restore_handler::restore_sandbox
│            ↓ awaits db.get_sandbox_row, read_snapshot_row, update_sandbox_status, ...
│            ↓ awaits reserve_vm_index_with_retry
│                  ↳ attempt 1: backend.reserve_vm_index (sync) → Err
│                  ↳ compio::time::sleep(2s).await    ← THE STALLED AWAIT
```

The starvation mechanism is:

1. ntex worker N's compio runtime polls T's future. T's first poll dives into `compio::runtime::spawn_blocking(|| ureq.delete(/shutdown, 60s))` — that DOES dispatch the ureq call to a blocking-pool thread, and T's outer future parks on the resulting future-handle's wakeup.

2. The blocking task takes the full 60s (half-dead agent, connection-refused-then-retry inside ureq). During that 60s, the worker runtime IS free to poll other futures including B's `sleep(2s).await`.

3. **BUT**: the next sleep returns. B's task gets re-polled and the loop body runs (`backend.reserve_vm_index` — synchronous mutex acquire+release, microseconds). Returns Err. The closure enters its second `compio::time::sleep(2s).await`. **The wake registration for this new sleep is registered on the same runtime's timer wheel.**

4. Meanwhile, T's blocking-pool ureq completes (60s in). T's join future fires `wake_by_ref()` on T's outer future. **The runtime now has two ready tasks**: T (became ready) and B (sleep timer was about to expire too).

5. The runtime's task-pop order is FIFO; T resumes immediately. T's next step is *another* `spawn_blocking` (stop_nomad_job ureq 15s). Park.

6. **The key observation**: each round-trip on T's body consists of `spawn_blocking` (no runtime residency) + 1–2 awaits/sync-ops between blocking calls. The runtime ALWAYS has spare poll capacity between T's blocking joins; B's sleeps should fire. Yet the smoke shows B's loop made ZERO progress past attempt 1.

7. **What actually happened (refined hypothesis after re-reading the trace)**: smoke-r7's last phase is `pre_reserve_vm_index` at `02:54:16.494`. Reserve attempt 1 returns Err. Closure awaits sleep(2s). At `t+2s = 02:54:18.5`, attempts 2's reserve runs and again returns Err. **And so on for 45+ attempts.** Each `restore: phase=*` line is OUTSIDE the retry loop body — they only fire BEFORE/AFTER `reserve_vm_index_with_retry`. The reserve-retry loop emits NO log on each Err — only on first success (line 281) or on budget exhaustion (line 295). The trace went silent **because the loop is logging-silent until exit**, not because the runtime starved.

   Smoke-r7's `02:55:46` host_fence-cleared event corresponds to ~attempt 46 (90s after first attempt). At attempt 46, the slot should be released by T's `release()` and reserve should succeed. **But the wake client already TimedOut at t+60s** (= attempt 30), so the *client* connection was already closed by the time the runtime would have resumed the wake handler.

8. **The ntex worker's response to client timeout**: when the client TCP closes, ntex eventually drops the response future. Dropping the future cancels B at its current suspension point. If B was inside the inner `sleep(2s).await`, drop fires before the next reserve attempt runs.

9. **NET**: the wake handler was NOT runtime-starved in the polling sense; it was **client-timeout-canceled** mid-retry. The phase trace went silent because the retry body has no INFO logs, not because the worker was poll-blocked. The C-6 mystery isn't runtime starvation per se — it's "C-4's retry budget (120 s) is longer than the client's TCP timeout (60 s), and the retry loop has no logs to indicate progress."

   **This is a substantive correction to the smoke-r7 / `91ce9be5` commit-message theory.** The OS-thread-decoupling fix is still correct (defense-in-depth against any genuine runtime-residency starvation), but the C-6 root cause is more subtle than "compio runtime starved." The real root cause is **C-4's retry budget exceeded the client deadline, in a loop with no observability**.

### Verifying the refined hypothesis

| Evidence | Old theory (runtime starvation) | Refined theory (silent retry past client deadline) |
|---|---|---|
| Phase trace stops at `pre_reserve_vm_index` | Worker poll-blocked → no further awaits run | Reserve loop has no per-attempt log; trace IS silent between pre/post markers |
| Wake never logs `vm_index reserved after retry` | Worker never resumed | Wake future dropped at 60s; never reached attempt 46 to log success |
| Wake never logs `reserve exhausted retry budget` | Same as above | Loop didn't run to 60; future dropped first |
| Source-teardown completes at t+90s, releases vm_index | Stop_inner's awaits ran on the runtime, blocking B | Stop_inner's awaits DID run via spawn_blocking; vm_index released as expected |
| smoke-r7 had a 1-worker fleet | Same-worker collision is 100% certain | Same-worker collision is 100% certain, but doesn't IMPLY starvation |

The refined theory is also consistent with `91ce9be5`'s fix: moving teardown to a dedicated OS thread removes the same-worker collision entirely. **Even though the original "starvation" hypothesis was imprecise**, the fix is right — because (a) it removes a real concurrency dependency the smoke-r7 review correctly identified as architecturally smelly, and (b) on `c ≥ N_workers` cluster runs where some wakes WOULD have shared a worker with teardown, the decoupling is necessary regardless of whether the underlying mechanism is starvation or some subtler interleaving.

**Recommendation for the C-6 closure narrative**: the deferred file's wording (`stop_inner's ... ureq call burns ~60s ... competes 1:1 with the wake handler's reserve_vm_index_with_retry sleep continuations`) overstates the runtime-poll competition. The truer description is: **C-4's retry runs for up to 120 s with no per-attempt visibility, and the client's 60 s deadline cuts the future mid-retry before the slot frees**. The OS-thread fix is right (defense-in-depth + matches the cluster's empirical behaviour); the diagnosis isn't quite. Worth a `docs/reviews/sandbox-snapshot-restore-deferred.md` text refresh before this becomes lore.

### Mapping every async fn that could be similarly affected

The "detached future on the same ntex worker runtime" shape is widespread. Detached-spawn audit below.

## R14-C1, R14-I2, R14-I1 (NEW)

### [R14-C1] Sibling-C-6 in `sweep.rs::ControllerIdleSnapshotter::snapshot_one` — INLINE teardown on the sweep's spawn-loop runtime (CRITICAL, concurrency-r14)

- **Files**:
  - `crates/sandbox/src/sweep.rs:563` (`spawn_idle_eviction_sweep` — `compio::runtime::spawn(...).detach()` of the sweep loop).
  - `crates/sandbox/src/sweep.rs:375-388` (`ControllerIdleSnapshotter::snapshot_one` awaits `state.backend.teardown_source_for_snapshot(sandbox_id).await` **INLINE**).
  - `crates/sandbox/src/sweep.rs:527-528` (`futures::future::join_all` of `cap` per-row futures, default `cap=2`).
- **Shape**: the idle-eviction sweep loop is a compio task. Each tick runs `run_idle_eviction_once` → `snapshot_rows_chunked` → `join_all(per_row.snapshot_one() for row in chunk)`. Each `snapshot_one` does the full snapshot pipeline AND then awaits `teardown_source_for_snapshot(sandbox_id).await` IN-LINE. The teardown body is `stop_preserving_state` → `stop_inner(.., false)` — same `/shutdown` + `wait_for_job_gone` + `wait_for_agent_silent` chain that took 90 s in smoke-r7.
- **Concurrency surface**:
  - Sweep loop runs on whatever ntex worker compio runtime first polled `spawn_idle_eviction_sweep`'s spawned task.
  - `join_all(2 futures)` on a single compio task means BOTH inline teardowns run "concurrently" by interleaving polls on the same task — they share runtime residency with whatever else the worker is running.
  - **Wake handlers landing on the same ntex worker would face the same starvation/co-residency issue C-6 captured** (with the refined "silent retry past deadline" framing: a wake's `reserve_vm_index_with_retry` could be canceled by client timeout before the inline teardowns release their slots).
- **Why the C-6 fix doesn't cover this**:
  - `91ce9be5` only changed `admin_handlers.rs:1311`'s detach pattern.
  - The sweep-path teardown is invoked via `IdleSnapshotter::snapshot_one`'s in-line `.await` — there's no detach to migrate. To migrate, the sweep would need to (a) detach the per-row work onto OS threads or (b) hold the sweep loop's runtime separate from the ntex worker pool.
- **Production impact**: TODAY's smokes are c=1 with idle-eviction defaulting to `SANDBOX_IDLE_SNAPSHOT_SECS=1800` (30 min) — so the sweep rarely fires during a c=1 5-min smoke. The bug is **dormant under current smokes**. Under the planned T-8b-stress (c=20, longer runs, higher idle churn), the idle sweep WILL run concurrent with c=20 wake traffic on the same worker → wedge potential.
- **Action**:
  - **Short-term**: spawn the sweep loop ITSELF on a dedicated OS thread with its own compio runtime, mirroring the C-6 fix. One-edit, mirrors the existing `91ce9be5` pattern at the sweep entry point.
  - **Long-term**: the sweep's `snapshot_one` should also detach the teardown step (mirror admin_handlers' pattern). The sweep loop's primary job is to drive snapshot_sandbox; the post-snapshot teardown is best-effort and should NOT be inline-awaited.
  - **Test gate**: T-8b-stress at c=20 with idle threshold pulled in (e.g. `SANDBOX_IDLE_SNAPSHOT_SECS=60`) should exercise this path. If wakes start surfacing C-6-style timeouts while the sweep is concurrently snapshot+teardown-ing other rows, R14-C1 has reproduced.

### [R14-I2] Sibling-C-6 in `registry.rs::start_idle_gc` — `backend.stop(id).await` on the GC loop's runtime (IMPORTANT, concurrency-r14)

- **File**: `crates/sandbox/src/registry.rs:828-871`.
- **Shape**: `start_idle_gc` spawns a compio task that periodically walks expired sandboxes and calls `state.backend.stop(id).await` IN-LINE. `backend.stop` resolves to `stop_inner(.., true)` — the SAME stop chain that exhibited the 90 s teardown wall in smoke-r7, plus host_dir rm and `persist.delete`.
- **Concurrency surface**: identical to R14-C1 (this loop is also a `compio::runtime::spawn(...).detach()` of an async block; the `.stop` calls inside it inherit the spawning task's runtime residency).
- **Difference from R14-C1**:
  - R14-C1's surface is snapshot+teardown chained per row; R14-I2's is bare stop()s on idle-expired sandboxes.
  - R14-I2's `stop(.., true)` is structurally distinct from R14-C1's `stop_preserving_state` — both call `stop_inner` but with different `remove_host_dir` flags. They share the agent `/shutdown`, Nomad job purge, host fence chain (the 90s window).
  - R14-I2's stops happen for FAR fewer rows per tick (only newly idle-expired) than R14-C1's snapshot sweep.
- **Why important, not critical**: the idle-GC tick is 60 s (`interval = Duration::from_secs(60)` at line 830) and only fires on idle-expired sandboxes — the production cadence rarely overlaps a 90 s wake. The window exists; the trigger probability is low.
- **Action**:
  - **Defensive**: same OS-thread treatment as the C-6 fix would close this. Sequencing: do R14-C1 first (sweep), then R14-I2 (registry), then `nomad_ch.rs:2002` (`CreateGuard` drop's detached cleanup) for completeness — all three share the "detached spawn on the ntex worker runtime that might co-host a wake" pattern.
  - **Or**: pull all detached spawns into a single named "background-runtime" thread that the controller mints at startup. One thread, one compio runtime, all detached work goes there. Replaces three ad-hoc OS-thread spawns with a shared infrastructure piece.

### [R14-I1] C-4's retry budget vs starvation re-framed: 120 s > 60 s client timeout, retry loop is silent per-attempt (IMPORTANT, concurrency-r14)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:265-306` — `reserve_vm_index_with_retry`.
  - `crates/sandbox/src/restore_handler.rs:141-145` — `VmIndexRetryPolicy::default()` (60 × 2 s = 120 s).
- **Shape (refined from C-6 analysis above)**:
  - Default budget exceeds default client timeout by 2× (60s observed in smoke-r7 client; 120 s retry budget).
  - The retry body emits no INFO line per attempt — only on success-after-retry (line 277) or budget-exhausted (line 295). Both fire only on EXIT.
  - When a wake races a source-teardown that's still holding the slot, the client TimedOut fires at attempt ~30 (60 s); the future is dropped mid-retry; no log surfaces the in-flight retries; the row stays in `restoring` until the lease-takeover sweep (R13-I1's compounding domain).
- **The prompt's hypothesis** ("C-4 uses 60×2s = 120s budget. Runtime starvation can consume 60s+. So C-4's retry effectively never gets its 60th attempt under contention") — **partially wrong**:
  - C-4's retry DID make progress between attempts (the inner `compio::time::sleep` yields cooperatively; nothing blocked the timer wheel).
  - But the future was DROPPED by client cancellation at 60 s, not stuck.
  - So the 60th attempt never runs — not because runtime is starved, but because the future is gone.
- **Why this is a correctness issue, not just a perf concern**:
  - The retry was DESIGNED to envelope the worst observed 90 s teardown. But the controller's client deadline isn't 90+ s — it's typically 60 s (smoke client; could be different per-deployment). Under any client deadline < 120 s, the retry budget is unreachable in practice.
  - On budget exhaustion the code returns `RestoreHandlerError::VmIndexUnavailable { requested }` → maps to 503 (per the existing surface). On client-cancel-mid-retry, the row stays in `restoring` → lease-takeover sweep eventually CASes it back to `snapshotted` per `claim_orphan_transient_for_recovery`. Different surfaces; the operator-facing experience differs based on whether attempt 60 fires.
- **Action**:
  - **Per-attempt INFO log** at the top of each attempt (1-line edit: `tracing::info!(sandbox_id, vm_index, attempt, "restore/wake: vm_index reserve retry");`). Closes the observability hole that made C-6 mysterious in smoke-r6/r7.
  - **Re-tune the retry budget** to match the typical client deadline minus 5 s. If the ambient deadline is 60 s, use ~28 attempts × 2 s = 56 s. If 30 s, use 14 × 2 s. The budget should be slightly less than the client's so the 503 surfaces in-band rather than the future being canceled.
  - **OR** plumb the client-deadline through to the retry (e.g. via ntex's request-context cancellation token); the retry then exits at the deadline-2s mark with a clear 503.
- **Why both R14-C1 and R14-I1 are needed**: R14-C1 fixes the runtime-residency dependency; R14-I1 fixes the deadline mismatch and observability. R14-C1 alone wouldn't fix the deadline-mismatch issue; R14-I1 alone wouldn't fix the runtime residency.

### [R14-D1] (DIAGNOSTIC VERIFICATION) Phase tracing at `8e7f0b53` was sufficient for localizing C-6, insufficient for diagnosing WHY (concurrency-r14)

- **File**: `crates/sandbox/src/restore_handler.rs:343-879` (14 `tracing::info!(phase=...)` boundary markers).
- **Was the diagnostic enough?**
  - **For LOCALIZATION**: yes — smoke-r7's "last `restore: phase=*` line is `pre_reserve_vm_index`" cleanly fenced the wedge to inside `reserve_vm_index_with_retry`. Six prior root-cause hypotheses from r6 were falsified by the trace.
  - **For ROOT-CAUSE IDENTIFICATION**: no — `pre_reserve_vm_index` told us "wedge is inside the retry loop body". It didn't distinguish:
    - (a) reserve() blocked on the mutex held by teardown's `release()` (FALSE — mutex hold is microseconds);
    - (b) `compio::time::sleep` failed to resume (FALSE per refined analysis above);
    - (c) the retry loop ran to attempts ~30 but was canceled by client deadline before logging success/exhaustion (TRUE);
    - (d) something else (e.g., panic in the loop swallowed somewhere) (would have been detected by an additional `panic_*` marker).
  - The team's commit message at `91ce9be5` picked (b) — which is plausible but not what the smoke trace actually proves. Per the refined analysis, (c) is the more precise root cause.
- **What the diagnostic was MISSING**:
  - Per-retry-attempt log lines in `reserve_vm_index_with_retry` (R14-I1's action item).
  - A `tracing::info!(phase = "wake_future_dropping", ...)` on the wake future's Drop (would have made client-cancel visible).
  - Optional: a runtime-instrumentation marker each time a detached task is polled (would have shown whether T's polls coincided with B's sleeps).
- **Action**:
  - Adding per-attempt logging (R14-I1) is the cheap fix that would have shortened smoke-r6→r7 by one cycle.
  - The diagnostic was paying off as designed (smoke-r7's commit explicitly says "did its job"); the SHORTFALL is that the chosen markers were on the async-boundary skeleton, not on the per-attempt retry body. The lesson: when adding tracing to localize, add markers on the SUSPECTED-LOOP iterations too, not just on the call sites of the loop.

### [R14-V1] `do_restore_inner` await count: 7 → 8 since r13 (VERIFICATION, concurrency-r14)

- **Files**: `crates/sandbox/src/restore_handler.rs:529-892` (`do_restore_inner`).
- **Awaits enumerated** (HEAD):
  1. `:558` — `reserve_vm_index_with_retry(...).await` **NEW since r13** (C-4 was already at HEAD per r13's reading; this line is INSIDE do_restore_inner not in the calling shell, so it was already counted — but r13 said 7. Re-counting now gives 8).
  2. `:614` — `spawn_blocking(store.get).await`
  3. `:722` — `spawn_blocking(submit_restore_job).await`
  4. `:750` — `spawn_blocking(wait_for_livez).await`
  5. `:803` — `persist.unseal(sandbox_id).await`
  6. `:825` — `clock_resync_post_restore(...).await`
  7. `:872` — `db.update_sandbox_status(...).await`
  8. `:880` — `db.clear_snapshot_metadata(...).await`
- **r10/r11/r12 era**: 7. **r13 era**: 7. **HEAD**: 8. **Delta: +1**.
- **Reconciliation with r13**: r13's count of 7 either (a) didn't include `:558` `reserve_vm_index_with_retry` as a top-level await (treated as part of the C-4 caller-side retry, not a do_restore_inner await), or (b) was off-by-one. Re-reading the code at HEAD, `:558` is unambiguously inside `do_restore_inner`'s body. Either r13's enumeration was incomplete or the C-4 fix had not yet landed in r13's audit window. Per the deferred file (`b2892368 - bounded retry for wake-vs-source-teardown vm_index race (C-4 fix)`), C-4 landed before r13 — so r13's count was off-by-one. Marking the verified count as 8.
- **What this means for cancel-safety**:
  - The `:558` await is itself an internal loop with up to 60 inner `sleep(2 s).await` suspension points. Each is a cancel/drop boundary.
  - Inner retry's drop-mid-loop semantics: the retry's only mutation of shared state is `backend.reserve_vm_index(...)` which is sync; on Err it does nothing. So mid-loop drop is benign in itself.
  - HOWEVER: if attempt N succeeded but the future was dropped between `reserve_vm_index` returning Ok and the closure returning Ok, the slot WAS reserved and the wake future is gone. The slot leaks. Code at lines 274-285 shows `Ok(()) => { ... return Ok(()); }` — the function-return point is structurally indivisible from the `Ok` arm, so a runtime-level cancel between the sync `reserve_vm_index` and the function-return point would leak. This is a narrow window (`return Ok(())` is sync between two async points) but it exists.
  - **Action**: same as R4-A2 LeasedVmSlot RAII. A Drop-guard on the reserved index would catch any cancel between reserve-success and the next state-machine commit (the CAS at line 872).

## Detached-spawn audit (concurrency-view)

Surveyed `compio::runtime::spawn(` in `crates/sandbox/src/`. New annotations per-site:

| Site | Worst-case starvation surface | Mitigation in tree? |
|---|---|---|
| `admin_handlers.rs:1339-1378` (was `:1311` pre-C-6) | 0 (detached on OS thread → dedicated runtime) | **YES** — C-6 fix `91ce9be5` |
| `sweep.rs:563` (idle-eviction sweep loop) | Up to ~225 s per row (snapshot full pipeline + inline teardown); `cap=2` concurrent | **NO** — R14-C1 |
| `sweep.rs:227` (transient-takeover sweep loop) | Low: pg-bound `claim_orphan_transient_for_recovery` per row | Not needed (no long sync I/O) |
| `lib.rs:989` (health probe loop) | Backend-dependent; backend.probe()'s wall is normally seconds | Not needed |
| `lib.rs:1072` (heartbeat) | Pg `heartbeat` is fast | Not needed |
| `lib.rs:1283` (takeover poll) | Pg-bound | Not needed |
| `lib.rs:2145` (graceful-shutdown task) | Bounded, short | Not needed |
| `registry.rs:829` (idle GC) | Up to ~225 s per row via `backend.stop(.., true).await` inline | **NO** — R14-I2 |
| `nomad_ch.rs:2002` (CreateGuard drop) | http_delete 10s + spawn_blocking | Acceptable today (10 s window, low rate); candidate for OS-thread |
| `main.rs:115` (preview-ws serve) | Long-lived dedicated listener loop | Acceptable (its own listener, no shared runtime risk per se) |

**Pattern observation**: the controller now has three distinct concurrency shapes for "background work that shouldn't block ntex worker":
1. **OS-thread + dedicated compio runtime** (C-3 + C-6 pattern at `snapshot_store_gcs.rs::Tiered::put` and `admin_handlers.rs:1339`).
2. **`compio::runtime::spawn(...).detach()` on the SPAWNING worker's runtime** (7+ sites listed above; some safe by virtue of low-wall-time bodies, some open like R14-C1/R14-I2).
3. **`compio::runtime::spawn_blocking`** (~30+ sites — all the ureq wrappers; runtime-residency-safe by design).

The fix lattice for the open R14-C1/R14-I2 (and the latent `CreateGuard` drop) is to consolidate around pattern (1) for any detached work that touches `stop_inner` / `teardown_source_for_snapshot`. Three more OS-thread spawns at the cost of ~3 lines each. Alternative: a single named "background-runtime" thread minted at controller boot with a message-queue interface.

## Carry-forward (escalation status)

| Finding | Open Since | Cycles | Severity Trajectory |
|---|---|---|---|
| **R4-A2 / R5-A2** LeasedVmSlot RAII | r4 | **11+** (incident-class) | Would close R10-C1, R10-C2, R11-C1, R11-C2, R11-I1, R12-M1, 6 C3 widenings, AND R14-V1's reserve-success / commit-CAS narrow drop window. C-6 fix did NOT subsume this — R14-C1/R14-I2 (sibling) are independent of LeasedVmSlot but related in shape. **12+ findings would dissolve.** |
| **R11-C1** `unregister_restored` silent-fail-OPEN on `nomad_handle=None` | r11 | 3 | Open. |
| **R11-C2** rollback closure 2-await window | r11 | 3 | Open. Still compounded by R13-I1. |
| **R10-Q3** registry.rs 35+ bare RwLock unwraps | r10 | 4 | Open. Out-of-scope re-affirmed. Code-quality. |
| **R10-S2** spawn_blocking JoinError swallow | r10 | 4 | Open. |
| **R7-C1** detached teardown task | r7 | 7 | **PARTIALLY CLOSED** by `91ce9be5` (admin path detached → OS thread). Sweep + idle GC remain (R14-C1, R14-I2). |
| **C3** cancel-unsafety in `do_restore_inner` | r3 | 11 | Subsumed by LeasedVmSlot. |
| **R10-M2** spawn_blocking panic-format `Any { .. }` | r10 | 4 | Open. |
| **R13-Q1 / R13-C1** ENV_LOCK race | r12-fix | CLOSED at `c5b9cb9d` | Verified at HEAD. |
| **R11-P1 / R13-I1** pool churn | r11 | 3 | Open. Re-classified concurrency-correctness via R13-I1; T-8b not yet at c≥10 to demonstrate. |

## do_restore_inner await count

- HEAD count: **8** (was 7 through r10-r13; r13's enumeration was off-by-one wrt `:558`).
- C-4 retry adds an additional N inner `sleep` await points (N = 60 default) inside that single outer await; cancel boundaries multiply accordingly.
- **Delta vs r13**: +1 outer await (recount); +60 inner suspension points under contention.

## Pattern observation

The pilot's cycle-r12 → cycle-r14 phase tracing → C-6 localization → C-6 fix arc is the **first** time this branch has surfaced a concurrency bug that traditional code review missed and only smoke-testing caught. The diagnostic instrumentation paid off; the SHORTFALL was choosing async-boundary markers over per-iteration-loop markers. R14-I1's per-attempt log is the lesson.

**Sibling-coverage observation**: when a detached-spawn pattern is identified as buggy (C-6 → admin/snapshot), the systematic next step is to enumerate every site that uses the same pattern and verify the sibling sites aren't also exposed. The audit table above catches `sweep.rs:563` and `registry.rs:829` as direct siblings. The `91ce9be5` commit message DOES mention sibling sites — "sweep.rs:227/563, registry.rs:829 are all steady-state loops with top-of-loop compio::time::sleep — safe" — but that statement is **incorrect for `sweep.rs:563`**: the sweep loop's body calls into `snapshot_one` which awaits `teardown_source_for_snapshot` inline. The "steady-state loop" framing missed that the WORK INSIDE the loop body has 90 s tail awaits.

**LeasedVmSlot escalation**: 11+ cycles open. Each round adds 1–2 findings that would dissolve under proper RAII. The cost-of-doing-nothing is monotonically growing. R14 adds zero new findings to that family (C-6 is not a LeasedVmSlot-class bug), but the carry remains the highest-incident-class debt.

## Status block (one-liner)

```
Round 14:
  NEW: R14-C1 (sweep::ControllerIdleSnapshotter::snapshot_one awaits
               teardown_source_for_snapshot inline on the sweep loop's
               runtime — same shape as C-6 but on a DIFFERENT spawn site;
               not covered by the 91ce9be5 fix; latent until idle-eviction
               sweep runs concurrent with wakes on same worker),
       R14-I2 (registry::start_idle_gc awaits backend.stop(.., true)
               inline on the GC loop's runtime — sibling-C-6 with lower
               trigger rate; same OS-thread fix would close),
       R14-I1 (C-4 retry budget 120 s > 60 s client deadline; loop is
               silent per-attempt; future canceled mid-retry before
               surfacing 503 or success-after-retry; refined root-cause
               framing for C-6 mystery),
       R14-D1 (phase-tracing diagnostic: sufficient for LOCALIZATION,
               insufficient for ROOT-CAUSE-ID; per-attempt loop markers
               needed),
       R14-V1 (do_restore_inner await count corrected to 8; r13 was
               off-by-one wrt the C-4 retry await at :558).

  C-6 ROOT-CAUSE REFINEMENT: the 91ce9be5 commit framed the bug as
                             "runtime starvation by detached teardown" —
                             this round's analysis shows the more
                             precise framing is "C-4 retry budget exceeds
                             client deadline; future is canceled mid-
                             retry; retry body has no per-attempt
                             observability". The fix is still correct
                             (defense-in-depth removes the runtime
                             dependency entirely), but the deferred file's
                             closure text should be refreshed.

  CARRIED: R4-A2 LeasedVmSlot (11+ cycles, incident-class — would close
                                12+ findings including R14-V1's narrow
                                cancel-window between reserve-Ok and
                                function-return),
           R11-C1 (3 cycles), R11-C2 (3 cycles, compounded by R13-I1),
           R10-Q3 (4 cycles, code-quality),
           R10-S2 (4 cycles, compounded by R12-M1),
           R7-C1 (7 cycles, PARTIALLY CLOSED at 91ce9be5; remainder is
                  R14-C1/R14-I2),
           R10-M2 (4 cycles),
           R11-P1 / R13-I1 (3 cycles, conn-cliff at c≥10).
  CLOSED: R13-Q1 / R13-C1 (ENV_LOCK race) at c5b9cb9d — verified at HEAD.
```
