# Sandbox/snapshot-restore — concurrency r31 review

Date: 2026-05-25 (UTC).
HEAD at audit: `17fc24b8` (worktree `.worktrees/sandbox-snapshot-restore`,
branch `feat/sandbox-snapshot-restore`).
Round 31 of N. READ-ONLY.

Scope since r30 (`0ee106d2` → `17fc24b8`, four targeted commits):

- **`ade8fb46`** r30-A1 — `NomadStopPermits` semaphore on `AppState` +
  `NomadCHBackend` (cycle 45). Closes the per-loop-cap composition
  failure flagged as CRITICAL by concurrency-r30.
- **`cdcd670d`** T-7+T-8 cutover — deleted `TaskDriverMode` enum +
  `raw_exec` wrapper path (cycle 47).
- **`b2acaf9d`** cadence quick-win — 150→50ms livez poll,
  250→100ms alloc_running poll (cycle 48). **On `feat/nomad-driver-ch`
  branch only; NOT yet merged into `feat/sandbox-snapshot-restore`.**
- **`b75728ce`** R31-P1 — `vm_index_ceil` default 12→20,
  `vm_index_release_delay_secs` default 5s→2s (cycle 48).

Focus this round:
- Cadence tightening (`b2acaf9d`): new races exposed at 50ms/100ms vs
  prior 150ms/250ms?
- 5s→2s `vm_index_release_delay_secs`: r24-A2-S3 race window now
  visible? Controller expectation tighter after driver v24 OFD-probe
  fix.
- Post-cutover state: `raw_exec` wrapper path deleted; any stale async
  patterns that assumed the dual-mode branch?
- R30-I1 carry: `catch_unwind` in `AppStateGcStopper::stop_one` — open.

Prior: `…concurrency-2026-05-25-r30.md`.

## Summary

- **4 findings this round** (0 NEW CRITICAL, 1 NEW IMPORTANT, 2 NEW
  MINOR, 1 verification-clean close).
- `ade8fb46` semaphore verified concurrency-clean — see `[N/A]` below.
  R30-CRITICAL-A1 is CLOSED.
- `b75728ce` 5s→2s `vm_index_release_delay_secs` — new race window for
  the r24-A2-S3 tap-cleanup settling? **NO new race:** driver v24
  OFD-probe fix confirmed `destroy_task_lock_held_total=0` across all
  measured stops; the 3s margin (2s delay minus observed <1s
  tap-cleanup max) is net-adequate. But three docstrings across two
  files still say "default 5 s" — see R31-M1 and R31-M2.
- `b2acaf9d` cadence tightening: NOT merged into
  `feat/sandbox-snapshot-restore` yet. Reviewed as pending. Found
  **R31-I1**: async `wait_for_alloc_running` in `nomad_ch.rs` has a
  250ms sleep in its parse-error branch (line 3065) that the diff did
  NOT touch. The two sleep sites in the function are inconsistent
  post-patch: parse-error branch stays at 250ms while the normal
  end-of-loop branch drops to 100ms. Not a race but a correctness /
  documentation gap.
- R30-I1 `gc_stop_chunked` join_all panic blast amplification:
  STILL OPEN — no `catch_unwind` was added to `AppStateGcStopper::
  stop_one`'s `backend.stop(id).await` call.

## IMPORTANT

### [R31-I1] (NEW IMPORTANT) `b2acaf9d` leaves `wait_for_alloc_running`'s parse-error sleep at 250ms — inconsistency across the two sleep sites in the same function

- **File** (on `feat/nomad-driver-ch`, pending merge):
  - `crates/sandbox/src/backend/nomad_ch.rs:3064-3065` — parse-error
    branch:
    ```rust
    last_parse_err = Some(msg);
    compio::time::sleep(Duration::from_millis(250)).await;
    continue;
    ```
  - `crates/sandbox/src/backend/nomad_ch.rs:3154-3155` — normal
    end-of-loop sleep (changed by `b2acaf9d`):
    ```rust
    compio::time::sleep(Duration::from_millis(250)).await;  // pre-b2acaf9d
    compio::time::sleep(Duration::from_millis(100)).await;  // post-b2acaf9d
    ```

- **The shape**:

  `wait_for_alloc_running` (async, used on the CREATE + cold-boot wake
  path) has two `compio::time::sleep` sites. `b2acaf9d` updated only
  the end-of-loop sleep (line 3154 → 100ms). The parse-error branch
  at line 3065 (entered when Nomad returns 200 but the body is not
  valid JSON) was NOT updated. Post-merge, Nomad JSON-parse failures
  during alloc-polling will still wait 250ms between retries, while
  normal polling waits 100ms.

  Under a Nomad garbage-response episode (e.g. a truncated HTTP body
  during a hot shard restart), the two sleep sites produce inconsistent
  retry cadences: the normal "no allocs yet" path probes at 100ms
  (10 probes/s) while the "got garbage" path probes at 250ms (4
  probes/s). Both are within latency budget (120s `alloc_running_
  timeout`), so this is NOT a correctness defect. But:

  1. The commit message claims "250→100ms alloc_running" across the
     board; the partial update contradicts the claim.
  2. The `wait_for_alloc_running_blocking` counterpart in
     `restore_handler.rs` (the `spawn_blocking`-hosted sync version
     used on the WAKE path) WAS updated consistently (both its single
     end-of-loop sleep and its function docstring were changed to
     100ms by `b2acaf9d`). The async version's second sleep site is
     the outlier.

  **Concurrency impact**: zero — the parse-error path is a single
  in-flight future; the sleep duration doesn't introduce a race.
  The concern is documentation accuracy and operational predictability
  (operators tuning `alloc_running_timeout_secs` against the poll
  cadence expect one cadence, not two depending on Nomad response
  shape).

  **Potential missed timing implication on tighter cadence**: at
  100ms normal cadence with 16 parallel stops (semaphore cap) each
  issuing their own `wait_for_alloc_running`, the controller
  generates up to 160 Nomad HTTP GETs per second on the CREATE hot
  path — up from 64 GETs/s at 250ms. On a single-node worker with
  a local Nomad agent this is fine. But if under Nomad parse-error
  stress (truncated bodies), the 250ms parse-error branch acts as
  an accidental back-off that protects against a "fast-retry into a
  broken Nomad" thundering-herd. Removing it (by making it 100ms
  too) removes the implicit back-off on the error path. Not a
  correctness issue; documented here for the merge decision.

- **Severity**: IMPORTANT (documentation/correctness gap in a pending
  merge; the partial-update is observable in production behaviour
  under Nomad parse-error stress; the commit message claim is
  inaccurate).

- **Recommendations** (reviewer-only):

  1. Before merging `b2acaf9d` into `feat/sandbox-snapshot-restore`,
     also update line 3065's `from_millis(250)` to `from_millis(100)`
     (or to a deliberate back-off value like 200ms with a comment).
  2. Update the `wait_for_alloc_running` function doc to note the
     parse-error branch cadence if it intentionally differs.

- **Carry mechanism**: NEW for r31; CLOSES with `b2acaf9d` merge.

## MINOR

### [R31-M1] (NEW MINOR) Three docstrings across two files still say "production default 5 s" after `b75728ce` lowered `vm_index_release_delay_secs` to 2s

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:2452-2453` —
    `CreateGuard.release_delay` field comment:
    ```rust
    /// T-8b-stress-r8 r24-A2-S3: the configured delay before
    /// releasing `vm_index` back to the allocator. Captured at
    /// guard construction so the detached drop task doesn't need
    /// to re-read the cfg; production default 5 s, 0 in tests.
    ```
  - `crates/sandbox/src/backend/nomad_ch.rs:1611-1612` — inline
    comment above `release_vm_index_after.await` in `stop_inner`:
    ```rust
    // Production default 5 s; 0 in tests via the
    // vm_index_release_delay_secs config knob.
    ```
  - `crates/sandbox/src/config.rs:354-355` — `vm_index_ceil`
    field rustdoc (a SEPARATE stale-doc issue, see R31-M2 below).

- **The shape**:

  `b75728ce` updated:
  - `config.rs` field rustdoc for `vm_index_release_delay_secs`
    (lines 447-454: correctly says "Default reduced 5 s → 2 s after
    v24 OFD-probe fix").
  - The two env-parse sites (`config.rs:953-954`, `gcp-worker-
    startup.sh:84`, `provision-gcp-cluster.sh:57`).

  It did NOT update:
  - `nomad_ch.rs:2452-2453` (`CreateGuard` field comment).
  - `nomad_ch.rs:1611-1612` (`stop_inner` inline comment).

  Both still say "5 s". An operator reading the inline comment in
  `stop_inner` to understand the settling budget sees a stale value.
  A reviewer diffing `nomad_ch.rs` sees an unexplained "5 s" that
  no longer matches the config-layer default.

- **Severity**: MINOR (documentation only; no race; no behavioral
  divergence). The config-layer default is authoritative; these
  are comment-layer artifacts.

- **Carry mechanism**: NEW for r31; trivial to fix (~2 LOC changes).

### [R31-M2] (NEW MINOR) `vm_index_ceil` field rustdoc still says "default 155" after `b75728ce` changed the env default to 20

- **File**:
  - `crates/sandbox/src/config.rs:354-355` —
    `NomadCHConfig::vm_index_ceil` field rustdoc:
    ```rust
    /// `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` (default
    /// 155).
    ```
  The env parse at `config.rs:931` now defaults to `20u16` (changed
  by `b75728ce`), and both startup scripts use `:-20`.

- **The shape**:

  A developer reading the field-level rustdoc (rendered in `cargo doc`
  or via IDE hover) sees "default 155" for `SANDBOX_NOMAD_CH_VM_INDEX_CEIL`.
  The actual default is 20. The maximum supported value (155 — the
  IP-arithmetic ceiling) is still correct as the UPPER BOUND; the
  confusion is between the max-safe ceiling and the env default.

  The body text of the rustdoc ("the 155-VM ceiling is plenty for
  single-host operators") is fine as documentation of the design
  ceiling; only the env-var-default line `(default 155)` is wrong.

- **Severity**: MINOR (documentation only; the validate() ceiling
  check at `vm_index_ceil <= 155` is unchanged and correct). The
  wrong default value in the field doc will confuse operators setting
  `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` who read the rustdoc instead of
  the env default at parse time.

- **Carry mechanism**: NEW for r31; trivial (~1 LOC change).

## Items NOT findings (verified clean this round)

### [N/A] `ade8fb46` `NomadStopPermits` semaphore — VERIFIED CONCURRENCY-CLEAN

**r30-CRITICAL-A1 is CLOSED.** The implementation is correct across
all five properties concurrency-r31 checked:

**(a) Acquire-before-shutdown ordering**: `stop_inner` acquires the
permit at `nomad_ch.rs:1465-1468`, AFTER the idempotent-re-stop check
(`state.write().remove(&sandbox_id)` at line 1440-1441 — already owned
in memory). The acquire is BEFORE the `/shutdown` POST at line 1492.
The `_permit` variable is bound until `stop_inner` returns (post
`persist.delete` tail). All 7 production teardown paths funnel through
`stop_inner`; the cap is structurally impossible to bypass. **Clean.**

**(b) Semaphore implementation (flume-channel pattern)**: `NomadStopPermits`
uses a `flume::bounded::<()>(capacity)` channel pre-loaded with
`capacity` tokens (in `new()`). `acquire()` calls `recv_async()` which
parks the caller until a token is available. `NomadStopPermitGuard::
drop()` calls `try_send(())` to return the token. The `Sender` clone
held by the guard (via `refill`) is the same channel endpoint as the
constructor's `tx` — a token returned by a guard refills the SAME
channel that future acquires drain. **No token duplication, no
disconnection risk (the `NomadStopPermits` struct holds `refill:
Sender<()>`, keeping the sender side alive for the lifetime of the
semaphore).**

The `expect` in `acquire()`:
```rust
self.tokens.recv_async().await.expect(
    "NomadStopPermits tokens channel is never disconnected…"
)
```
is sound — `self.refill` (a `Sender`) is held by `self` for as long
as the `NomadStopPermits` is alive. The channel's sender-half is only
fully dropped when ALL senders drop; the struct holds one permanently.
No spurious disconnect. **Clean.**

**(c) OnceLock install-once semantics**: `install_nomad_stop_permits`
uses `OnceLock::set` which is a no-op on the second call (returns
`Err(permits)` and drops the second `Arc`). The comment correctly
documents this as "silent no-op." The only caller in production is
`AppState::from_config`, which runs once per process. Tests that need
the integration go through `backend.install_nomad_stop_permits(…)`
explicitly; tests that don't exercise the cap correctly see `None`
from `stop_inner` and skip the acquire. **No race: `OnceLock` provides
a data-race-free publish-once / read-many contract across threads.**

**(d) AppState::new_fixture does NOT install the semaphore on the
backend**: by design (documented in the impl comment). The `Arc<
NomadStopPermits>` on `AppState.nomad_stop_permits` is sized from
config but the `NomadCHBackend`'s `OnceLock` is empty. Tests going
through `AppState::new_fixture` + `state.backend.stop(id)` skip the
semaphore. This is the intended "unit-test default" path. The missing
install means integration tests using `new_fixture` do NOT exercise
the global cap enforcement. **Not a concurrency defect** — this is a
test-coverage gap, not a correctness issue. (Cross-lens: test-coverage
r31 owns if a `new_fixture`-driven integration test is desired.)

**(e) Global gauge under concurrent acquires**: `dec_nomad_stop_permits_
in_use` uses a CAS loop with `saturating_sub` to avoid underflow/wrap.
The CAS loop is correct: it reads, computes `saturating_sub(1)`,
and retries on `compare_exchange_weak` failure. Under concurrent
`Drop` calls from multiple guards, each CAS loop contends on the
same atomic but each individually converges. No lost-decrement possible
because each guard owns exactly one `try_send` token and exactly one
`dec_*` call. **Clean.**

The test `nomad_stop_permits_in_use_gauge_decrements_on_release` uses
a delta pattern (`pre + 2`, `pre + 1`) to avoid hardcoded baseline
issues. Under parallel test execution (Rust's default test harness
is multi-threaded), concurrent tests that bump the same global
`NOMAD_STOP_PERMITS_IN_USE` atomic could cause false failures in this
test. However, the only other tests in the file that touch this metric
are also in the `r30-A1` block, and tests within a crate run
sequentially by default when `#[compio::test]` macros are used
(compio's test runtime is single-threaded). **Practically clean.**

Conclusion: **R30-CRITICAL-A1 CLOSED.** The semaphore is correctly
implemented and the integration-level `stop_inner_acquires_permit_
when_installed` test pins the load-bearing contract.

### [N/A] `cdcd670d` raw_exec deletion — VERIFIED CONCURRENCY-CLEAN

The T-7+T-8 cutover removes `TaskDriverMode`, `task_driver_mode_from_
env()`, `wrapper_path` from `NomadCHConfig`, and the `raw_exec` branch
from `build_nomad_job_json_with`. From a concurrency lens:

- The deleted `test_env_lock` module was a `Mutex<()>` shared between
  test files to serialize `std::env::set_var("SANDBOX_TASK_DRIVER", …)`
  calls. With the env-var and both code branches gone, the mutex has no
  remaining purpose. Deletion is correct.
- `build_nomad_job_json_with` now takes one fewer parameter (dropped
  `mode: TaskDriverMode`). Every call site updated in the same commit.
  No dangling references to the deleted enum or env-var reader.
- The residual mentions of `raw_exec` and `nomad-vm-wrapper.sh` in
  comments at `nomad_ch.rs:2410`, `3371-3372`, `snapshot_store.rs:315`,
  `restore_handler.rs:1227`, `1291`, `4794`, `4845`, `4967`, `4993`
  are historical context (describing what the driver replaced, or
  asserting that the `raw_exec command` field MUST NOT appear in the
  new ch-driver Config block). These are documentation artifacts, not
  live code paths. No concurrency surface change.

The two new tests (`nomad_job_spec_always_uses_ch_driver`,
`nomad_restore_job_spec_always_uses_ch_driver`) verify the
unconditional `Driver="ch"` emission at the spec-construction layer.
**Concurrency-clean: no shared state, no async surface, no lock.**

### [N/A] `b75728ce` 5s→2s `vm_index_release_delay_secs` — NO NEW RACE vs r24-A2-S3

The r30-M1 analysis established that the 5s `vm_index_release_delay_secs`
is the ONLY post-fence-cleared, pre-release barrier at the controller
side. Reducing it to 2s tightens that barrier. The question is whether
any race window now visible that 5s was masking.

**Driver-side evidence**: `b75728ce`'s commit message cites v24's
OFD-probe fix (`destroy_task_lock_held_total=0` across all measured
stops). The OFD-lock probe in `stop_task.go:464` was broken before
v24 — it opened `rootfs.img` with `O_RDONLY` and attempted `F_OFD_
SETLK F_WRLCK` (write lock on a read-only fd = `EBADF`). Every probe
returned EBADF, masking as "lock not held" even when CH held the lock.
The v24 fix changed to `O_RDWR` so the probe now correctly detects
held locks. With the probe working, the driver's destroy path correctly
blocks until CH has released `rootfs.img` before signalling
`host_fence_cleared`.

**The r24-A2-S3 race window**: after `host_fence_cleared`, the 2s
delay is the settling budget for:
1. Tap netdev eviction from the prior tenant's CH process.
2. fcntl lock drain from the prior tenant (now the driver's job).
3. OS scheduler settling for dead kernel threads.

For (2): with v24's correct OFD-probe, the driver already waits for
the fcntl locks to release before signalling `host_fence_cleared`.
So the 2s controller-side delay does NOT need to cover lock drain —
the driver already closed that window.

For (1): tap cleanup (`ip link delete zsbx-nm-<idx>`) is synchronous
in the driver's `destroyTask`. Observed tap-cleanup max < 1s per
`b75728ce` commit message. The 2s delay provides 1s of headroom
post-fence-cleared.

For (3): kernel thread scheduling is in the sub-millisecond range;
no concern at 2s.

**Concurrency conclusion**: NO new race window introduced. The 5s
budget was sized for the broken-probe era; with the probe working,
2s is a sound conservative estimate. The controller-side `reserve_
vm_index_with_retry` STILL blocks on `release_vm_index_after.await`
which STILL sleeps the full `vm_index_release_delay_secs` post-fence-
cleared before releasing the slot. The wake path cannot reach
`submit_restore_job` until AFTER the 2s delay elapses.

R30-M1 finding (that 5s is the only barrier) is updated: the barrier
is now 2s, and its sizing basis has changed from "empirical worst-case
for broken probe" to "empirical worst-case for tap cleanup with correct
probe." The c=4 rootfs.img `__fput` wedge documented in R30-M1 is NOT
affected by this change — that wedge occurs at the driver's destroy
phase BEFORE `host_fence_cleared` fires, and the driver's v24 OFD-probe
fix addressed exactly that. **Controller-side concurrency: clean.**

## Carry table (delta)

| Finding | Source | r31 state |
|---|---|---|
| R30-CRITICAL-A1 | r30 NEW-CRIT (concurrency-r30 deferred as arch ask) | **CLOSED at `ade8fb46`** (NomadStopPermits semaphore; verified concurrency-clean) |
| R30-I1 gc_stop_chunked join_all panic blast | r30 NEW-IMP | **STILL OPEN** (no catch_unwind added this cycle) |
| R30-M1 WAKE rootfs.img wedge controller analysis | r30 minor | **UPDATED** (2s delay is sound post-v24; wedge is driver-closed) |
| R30-M2 snap-idle-gc ignores shutdown_requested() | r30 minor | carry |
| R30-M3 state.sandboxes.get touches last_used in stop_one | r30 minor | carry |
| **R31-I1** b2acaf9d parse-error sleep still 250ms | r31 NEW-IMP | NEW (pending merge; closes with merge fixup) |
| **R31-M1** "default 5 s" stale in nomad_ch.rs comments × 2 | r31 minor | NEW |
| **R31-M2** vm_index_ceil rustdoc says "default 155" not 20 | r31 minor | NEW |
| Carry (older): R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1, R27-I1, R27-I2, R27-M1, R27-M2 | older | carry |

## Status block

```
Round 31 (ade8fb46 NomadStopPermits CLOSED; cdcd670d raw_exec deleted;
          b75728ce R31-P1 2s release-delay sound; b2acaf9d cadence
          tightening on feat/nomad-driver-ch, pending merge):

  CLOSED THIS ROUND:
    R30-CRITICAL-A1 NomadStopPermits global semaphore
      — CLOSED at ade8fb46 (verified concurrency-clean; 5
      properties; load-bearing test pins the contract).

  UPDATED ANALYSIS:
    R30-M1 WAKE rootfs.img controller-side analysis:
      — 5s→2s release delay is sound post-v24 OFD-probe fix.
        The 2s barrier is correctly sized for tap-cleanup (max
        observed <1s). The c=4 rootfs.img __fput wedge was
        driver-side and is CLOSED at driver v24. R30-M1 is
        now CLOSED with watchful-eye (monitor for any regression
        with the tighter default on c=4 stress).

  STILL OPEN (carry):
    R30-I1 gc_stop_chunked join_all panic blast amplification
      — no catch_unwind added to AppStateGcStopper::stop_one.
        Blast radius is 1→8 on panic in stop_one. FutureExt::
        catch_unwind recommendation from r30 still applies.

  NEW IMPORTANT:
    R31-I1 b2acaf9d wait_for_alloc_running parse-error sleep
      stuck at 250ms (the main loop-end sleep dropped to 100ms
      but the parse-error branch was not updated). Inconsistency
      contradicts commit message claim. Closes with merge fixup.

  NEW MINOR:
    R31-M1 "production default 5 s" stale in nomad_ch.rs
      inline comment (stop_inner) and CreateGuard field doc.
      Trivial fix (~2 LOC).
    R31-M2 vm_index_ceil rustdoc still says "default 155"
      after env default changed to 20. Trivial fix (~1 LOC).

  STILL OPEN (carry from r30):
    R20-I2 sweep host-scoping (12th cycle)
    R20-I3 Restoring watchdog (12th cycle)
    R24-I1 fsync_dir doc-misframe
    R25-I1 sweeper TOCTOU invariant untested
    R25-I2 Phase::Ok terminal-overwrite divergence
    R26-I1 boot-lookup serial-await + external-dep
    R27-I1 host_dir mtime baseline shifts
    R27-I2 home.img per-user race + Phase 5 latent
    R27-M1 host_dir_created semantic-overload
    R27-M2 wait_for_alloc_running budget shift (comment)
    R30-M2 snap-idle-gc shutdown_requested() discipline
    R30-M3 state.sandboxes.get touches last_used in stop_one

  ASK:
    (1) b2acaf9d merge fixup: update nomad_ch.rs:3065
        from_millis(250) → from_millis(100) (or document
        the intentional parse-error back-off). ~1 LOC.
    (2) R31-M1 fixup: update two "production default 5 s"
        comments in nomad_ch.rs to "production default 2 s".
        ~2 LOC.
    (3) R31-M2 fixup: update vm_index_ceil rustdoc
        "(default 155)" → "(default 20)". ~1 LOC.
    (4) R30-I1 (carry): per-id catch_unwind in
        AppStateGcStopper::stop_one around backend.stop(id).await.
        FutureExt::catch_unwind is the recommended shape (~15 LOC).
    (5) R30-M2 (carry): add shutdown_requested() check at top of
        start_idle_gc loop. ~3 LOC.
    (6) R30-M3 (carry): SandboxRegistry::peek (touch-free lookup)
        for GC log line in stop_one. ~10 LOC.
    (7) arch / carry: R27-I1, R27-I2, R26-I1 disposition — pending
        Phase 5 design call.
```

## ASK clarifications for the user

Three new open items from r31:

1. **R31-I1 merge gate**: when merging `b2acaf9d` into
   `feat/sandbox-snapshot-restore`, the parse-error sleep at
   `nomad_ch.rs:3065` should be updated to 100ms (matching the
   normal-path change). If a deliberate slower back-off is wanted on
   the parse-error path, document it explicitly with a comment like
   "slower cadence on bad-JSON path to avoid fast-retry into a
   broken Nomad response stream." Concurrency-r31 mildly prefers
   consistency (both 100ms) unless there is empirical evidence that
   the parse-error path needs a back-off.

2. **R31-M1/M2 doc fixup**: stale "5 s" and "155" defaults are two
   separate 1-LOC fixes. Suggest folding into the next config-layer
   PR or a dedicated doc-fixup commit before the next cluster stress.

3. **R30-I1 open**: the panic blast-radius amplification in
   `gc_stop_chunked` is the only IMPORTANT-level finding still open
   after this cycle. With the global semaphore in place (R30-A1 CLOSED),
   the GC path's `join_all` cap=8 is now limited to
   `min(8, nomad_stop_concurrency)` in-flight stops per chunk. The
   panic risk is reduced by the semaphore gating, but NOT eliminated
   — a panic inside a permitted `stop_one` future still drops up to
   7 sibling futures. The `FutureExt::catch_unwind` fix from r30's
   recommendation 1 is still the right close.
