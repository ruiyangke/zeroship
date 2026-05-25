# Sandbox/snapshot-restore — concurrency r32 review

Date: 2026-05-25 (UTC).
HEAD at audit: `8d82ecde` (worktree `.worktrees/sandbox-snapshot-restore`,
branch `feat/sandbox-snapshot-restore`).
Round 32 of N. READ-ONLY.

Scope since r31 (`17fc24b8` → `8d82ecde`):

- `a6e517b2` cadence cherry-pick — 150→50ms livez poll, 250→100ms
  `wait_for_alloc_running` end-of-loop sleep.
- `c56893b2` r31-S1 close — `driver.raw_exec.enable=1` removed from
  `gcp-worker-startup.sh` Nomad client `options` block.
- `4ac1e526` r31-I1 close — `wait_for_alloc_running` parse-error
  sleep at `nomad_ch.rs:3081` now 100ms (was 250ms).
- `1d58ab53` r32-T1 trace emits — `submit_done` + `alloc_first_seen`
  on the CREATE path.
- `8d82ecde` trace-review doc only (no code).

Focus this round:
- New `tracing::info!` emits in `create_sandbox` and
  `wait_for_alloc_running` — borrow/lifetime/race introduced?
- Cadence at 50ms / 100ms — does the tighter loop expose any wait
  the slower cadence masked?
- R30-I1 (`gc_stop_chunked` panic blast) carry.
- Post-cutover (no raw_exec, no wrapper) — concurrency invariants
  that depended on the wrapper?

Prior: `…concurrency-2026-05-25-r31.md`.

## Summary

- **3 findings** (0 NEW CRITICAL, 0 NEW IMPORTANT, 1 NEW MINOR,
  2 verification-clean closes).
- R31-I1 **CLOSED at `4ac1e526`** — both sleep sites in async
  `wait_for_alloc_running` are now 100ms; matches blocking sibling
  in `restore_handler.rs:2803`.
- r32-T1 trace emits are concurrency-clean: pure log events, no
  shared state, no borrow conflicts, no lock held across the log.
- R30-I1 **STILL OPEN** — async `state.backend.stop(id).await` at
  `registry.rs:975` is still unguarded; only the sync
  `state.sandboxes.remove(&id)` at line 982 is wrapped.
- raw_exec / wrapper removal: no concurrency invariant in the
  controller depended on the wrapper.

## IMPORTANT

(None new.)

## MINOR

### [R32-M1] (NEW MINOR) `alloc_first_seen` trace lacks `sandbox_id` — correlation gap on a parallel CREATE hot path

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3090-3094`
  ```rust
  tracing::info!(
      job = %job_id,
      elapsed_ms = %fn_started.elapsed().as_millis(),
      "sandbox/nomad-ch alloc_first_seen"
  );
  ```

  Companion `submit_done` emit at `:1189-1194` carries BOTH
  `sandbox_id = %sandbox_id` and `job = %job_id`.

- **Shape**: under N parallel CREATEs (c=4 stress), operators
  correlating by `sandbox_id` find every CREATE-path emit
  EXCEPT `alloc_first_seen` — they must first map
  sandbox_id → job_id via the `submit_done` line, then re-grep
  by `job`. `wait_for_alloc_running` is a free function not
  coupled to the `Sandbox` type, so `sandbox_id` isn't in scope;
  the fix is a ~3 LOC plumb of `sandbox_id: &Uuid` into the
  function signature.

- **Concurrency impact**: NONE — structured-logging consistency
  gap, not a race. Trace fires at-most-once via the
  `alloc_first_seen_logged` bool guard.

- **Severity**: MINOR (operability; high-traffic trace point).

- **Carry**: NEW for r32.

## Items NOT findings (verified clean this round)

### [N/A] r32-T1 trace emits — VERIFIED CONCURRENCY-CLEAN

**`submit_done` at `nomad_ch.rs:1186-1194`**: emitted right after
`guard.job_submitted = true`. `create_started: Instant` from
`:1005`, `sandbox_id: &Uuid` and `job_id: &str` already in scope.
Lock-free `Instant::elapsed()`. No new shared state, no new lock.
**Clean.**

**`alloc_first_seen` at `:3086-3096`**: emitted from the hot poll
loop when the JSON `allocs` array first becomes non-empty.

Borrow shape is sound:
- `alloc_arr: Option<&Vec<serde_json::Value>>` is `Copy` (Option of
  a reference). The `.map(|a| !a.is_empty())` reads via the ref,
  then `for a in alloc_arr.into_iter().flatten()` consumes the
  copied Option — no borrow conflict with the intervening trace.
- `alloc_first_seen_logged: bool` is a local stack variable on the
  single future; never crosses an `.await` boundary visible to
  another task. No `Sync`/`Send` contention.
- `fn_started: Instant` captured at function top (`:3045`); read
  on every emit. Lock-free.

Trace fires AT MOST ONCE per `wait_for_alloc_running` invocation
(one-shot guard). On c=4 with 100ms cadence, each CREATE emits
exactly one `submit_done` + one `alloc_first_seen`. Log volume
bounded. **Clean.**

### [N/A] 50ms / 100ms cadence — NO NEW RACE vs 150ms / 250ms prior

Two poll loops tightened:
1. `wait_for_agent_livez` 150→50ms (in-VM agent `/livez`).
2. `wait_for_alloc_running` 250→100ms (Nomad alloc-status, both
   sleep sites after `4ac1e526`).

Under c=16:

- **Nomad agent HTTP load**: 100ms cadence × 16 in-flight ≈ 160
  GETs/s against the local Nomad agent. Nomad in-process HTTP
  handles >5K req/s on a single host; 160 is well under saturation.
- **Agent `/livez` load**: 50ms × per-VM = 20 GETs/s per VM. The
  16-parallel fan-out lands on 16 DIFFERENT VMs (one per sandbox),
  not the same one. No back-pressure surface.
- **CompIO timer driver**: monotonic deadline heap (O(log n) insert,
  O(1) min-pop). 3× wake rate stays well below any data-structure
  threshold.
- **Race candidate**: a "transient state observed mid-cycle" race
  (e.g., `pending → running → failed` within one 50ms window).
  The poll's terminal states (`running` returns Ok; `failed`/`lost`
  return Err) are absorbing — faster polling can only REDUCE the
  chance of misclassifying a transient state, never create one.

Blocking sibling `wait_for_alloc_running_blocking` at
`restore_handler.rs:2803` already uses `thread::sleep(100ms)` for
its only sleep site; consistent with the async version after
`4ac1e526`. **Clean.**

### [N/A] `c56893b2` raw_exec.enable removal — VERIFIED CONCURRENCY-CLEAN

The controller's `build_nomad_job_json_with` (post-`cdcd670d`)
unconditionally emits `Driver = "ch"`. Removing the Nomad client's
ability to RUN raw_exec is dead-surface trimming; the controller's
behavior, shared state, locks, and async paths are all unchanged.
Negative-space asserts at `nomad_ch.rs:5649`, `:5815` and
`restore_handler.rs:4848`, `:4996` continue to pin the contract.

**Concurrency invariants that depended on the wrapper**: none.
`AppStateGcStopper`, `release_vm_index_after`, the
`host_fence_cleared` signal, and the `NomadStopPermits` semaphore
are all driver-agnostic. Tap cleanup was the wrapper's job in
raw_exec; the wrapper has been gone since `cdcd670d`. Tap cleanup
is now in the ch driver's `destroyTask` (driver-side, outside this
scope). **No carry-over invariant.**

## Carry table (delta)

| Finding | Source | r32 state |
|---|---|---|
| R31-I1 parse-error sleep 250→100ms | r31 IMP | **CLOSED at `4ac1e526`** |
| R30-I1 gc_stop_chunked join_all panic blast | r30 IMP | **STILL OPEN** (`registry.rs:975` `backend.stop(id).await` unguarded) |
| R30-M1 WAKE rootfs.img controller analysis | r30 minor | CLOSED with watchful-eye (per r31) |
| R30-M2 snap-idle-gc shutdown_requested() | r30 minor | carry |
| R30-M3 state.sandboxes.get touches last_used | r30 minor | carry |
| R31-M1 "default 5 s" stale comments | r31 minor | STILL OPEN |
| R31-M2 vm_index_ceil rustdoc "default 155" | r31 minor | STILL OPEN |
| **R32-M1** alloc_first_seen lacks sandbox_id | r32 minor | NEW |
| Older carry: R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1, R27-I1, R27-I2, R27-M1, R27-M2 | older | carry |

## Status block

```
Round 32 (a6e517b2 cadence merged; c56893b2 raw_exec.enable
          dropped; 4ac1e526 parse-error sleep fixed; 1d58ab53
          trace emits added):

  CLOSED THIS ROUND:
    R31-I1 wait_for_alloc_running parse-error sleep
      — CLOSED at 4ac1e526 (250→100ms; both sites consistent
      with blocking sibling).

  STILL OPEN (carry, unchanged):
    R30-I1 gc_stop_chunked join_all panic blast
      — async backend.stop(id).await at registry.rs:975
      still unguarded. Only sync remove() at :982 is
      wrapped. FutureExt::catch_unwind around the .await
      is still the recommended ~10 LOC fix.

  NEW MINOR:
    R32-M1 alloc_first_seen omits sandbox_id — plumb
      sandbox_id into wait_for_alloc_running. ~3 LOC.

  STILL OPEN (carry from r31):
    R31-M1 "production default 5 s" stale comments
    R31-M2 vm_index_ceil rustdoc "default 155"

  STILL OPEN (older carry):
    R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1,
    R27-I1, R27-I2, R27-M1, R27-M2, R30-M2, R30-M3

  ASK:
    (1) R32-M1: plumb sandbox_id into wait_for_alloc_running.
        ~3 LOC.
    (2) R30-I1 carry: catch_unwind around backend.stop(id)
        .await in AppStateGcStopper::stop_one. ~10 LOC.
    (3) R31-M1 carry: 5s → 2s comment fixups. ~2 LOC.
    (4) R31-M2 carry: vm_index_ceil rustdoc default fix.
        ~1 LOC.
```

## ASK clarifications for the user

One new open item from r32:

1. **R32-M1 alloc_first_seen trace**: the new `alloc_first_seen`
   info-emit lacks the `sandbox_id` field that the companion
   `submit_done` carries. Under parallel CREATE on c=4, operators
   correlating by sandbox_id lose the trace point unless they
   first translate sandbox_id → job_id via `submit_done`.

R30-I1 remains the only IMPORTANT-level finding open (no movement
this cycle). With the global `NomadStopPermits` semaphore from
r31, the panic blast radius is bounded but not zero — a panic
inside one `stop_one` future still drops sibling futures in the
same `join_all` chunk (≤7 at worst).
