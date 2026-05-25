# Sandbox/snapshot-restore — concurrency r17 review

Date: 2026-05-25 (UTC).
HEAD at audit: `163724dc` (branch `feat/sandbox-snapshot-restore`).
Round 17 of N. READ-ONLY.
Scope: PR2 partial (4 of 7 commits) — `wake_machine.rs`, dual-mode wake +
poll in `admin_handlers.rs`, `sweep.rs` wake_jobs GC, `tests/sandbox_pg_e2e.rs`
wake_machine_e2e fixtures.

Prior round: `sandbox-snapshot-restore-concurrency-2026-05-25-r16.md` (`2e9ae598`).

## Summary

- **8 NEW findings** (3 CRITICAL, 3 IMPORTANT, 2 MINOR), 0 closed.
- **R17-C1**: PR2 sets `lessee_updated_at` on INSERT but
  `update_wake_job_state` never bumps it. The wake_jobs takeover sweep
  promised by PR1's a0888d9e ("PR2 will wire the takeover-sweep that
  re-leases abandoned rows") is **ABSENT in PR2**.
- **R17-C2**: no UNIQUE INDEX on `wake_jobs.sandbox_id WHERE state NOT
  IN ('ok','failed')`. Concurrent POSTs both find None → both insert →
  both spawn machines → loser's `rollback_with` `teardown_restore`s,
  releasing the slot the winner just claimed (R10-C1 race shape).
- **R17-C3**: GC predicate `state IN ('ok','failed')` excludes mid-flight
  rows. With no takeover sweep, orphan rows persist forever AND block
  every subsequent wake via the idempotency check.
- **PR1 CRUD discipline VERIFIED SAFE** (prompt §6): no mutex across
  `pool.get().await` in any of the 5 methods.
- **Cancellation safety** (prompt §1): `detach_isolated` closes C-7 for
  the wake path (ntex disconnect immune) but process kill + R17-C1+C3
  leaks the row permanently.
- **No NEW `compio::runtime::spawn(...).detach()` in PR2** (prompt §5).
  Sibling-C-6 carry: `nomad_ch.rs:2002` `CreateGuard::drop` STILL on the
  shared worker runtime — R17-A5.

## Per-prompt-question audit

### Q1 — `wake_machine.rs` 728 LOC end-to-end

Transitions at: `:237` ReservingSlot, `:266` Restoring, `:358` LivezPolling,
`:381` ClockResyncing, `:413`/`:431` Registering, `:130-144` terminal Ok,
`:154-170` terminal Failed.

- **`lessee_updated_at` NOT bumped on any transition** (R17-C1).
  `set_state` (`:480-493`) → `update_wake_job_state` (`db.rs:3015-3054`),
  whose SQL writes `state`, `error_code`, `error_message`, `agent_url`,
  `updated_at` — NEVER `lessee_updated_at`. Lessee frozen at INSERT time.
- **Awaits NOT enclosed by a transition (work without state update)**:
  `reserve_vm_index_with_retry.await` (`:255` under ReservingSlot, may
  loop ~10s), `wait_for_livez` (`:367` under LivezPolling, ~30s),
  `clock_resync_post_restore.await` (`:400` under ClockResyncing, ~10s).
  Polling clients see one state for tens of seconds at a time.
- **Cancellation**: `detach_isolated` (`detach.rs:115-145`) uses
  `rt.block_on(make_fut())`. If the thread is killed mid-await the future
  drops at the current await point. pg row remains at the last `set_state`
  value. Combined with R17-C3 the row is leaked forever.
- **No shared `Arc<Mutex<_>>` crossing the thread boundary** that could
  deadlock with the main runtime. NomadCHBackend RwLock + vm_index_allocator
  Mutex are std::sync and never crossed `.await` (verified at `nomad_ch.rs:1671`
  register_restored body).

### Q2 — `admin_handlers.rs` dual-mode wake + poll

- **TOCTOU** between `find_pending_wake_for_sandbox` (`:1457`) and
  `insert_wake_job` (`:1495`): no transaction wrapping; no UNIQUE INDEX on
  `wake_jobs.sandbox_id` partial-WHERE non-terminal
  (`migrations/0009_wake_jobs.sql:106-108` is `CREATE INDEX`, not UNIQUE).
  → R17-C2.
- **`?sync=1` vs in-flight async**: sync path reads sandbox row, sees
  `restoring`, returns §10.0 `state_mismatch` 409 before any backend call.
  No deadlock. Safe.
- **Wake-id leakage probe** at `:1644-1656`: a mismatched-sandbox wake_id
  404s. Admin-bearer-gated. Non-issue.

### Q3 — `sweep.rs` wake_jobs GC

- Cadence `WAKE_JOBS_GC_POLL_SECS = 60` (`sweep.rs:81`); T_KEEP = 300s
  (`sweep.rs:88`) > sane client poll (1-5s). No "client misses terminal".
- GC vs active wake race: predicate `state IN ('ok','failed')`; terminal
  write is the machine's LAST pg call. No race window. Safe.
- BUT non-terminal rows are intentionally excluded; comment at
  `db.rs:3096-3097` defers them to "PR2's takeover scan". **Scan absent.**
  → R17-C3.

### Q4 — Stub fixtures (`tests/sandbox_pg_e2e.rs::wake_machine_e2e`)

6 new `#[ignore]`-gated tests covering happy / livez-fail / submit-fail /
reserve-fail / idempotency-invariant / GC-after-terminal.

**R16-T5 emergency hold SATISFIED for the wake path**:
`StubRestoreBackend::fail_submit` (line 4242) + `fail_livez` (line 4281) +
`fail_reserve` (line 4320) exercised. `fail_register` not added; all tests
use `persist: None` which skips the register triad → R17-I2.

### Q5 — Sibling-C-6 audit (carry)

- **R17-A5 confirmed**: `nomad_ch.rs:2001-2002` `CreateGuard::drop` STILL
  uses `compio::runtime::spawn(async ...)` on the spawning compio runtime
  (which on the create-failure path is the ntex worker). `catch_unwind` at
  `:2001` only guards process-teardown panic, not steady-state runtime sharing.
- **No NEW `compio::runtime::spawn(...).detach()` sites in PR2**: verified
  via `git diff 47e0251d..163724dc -- crates/sandbox/src/` grep for
  `^\+.*spawn|^\+.*detach`. All PR2 spawns are `detach_isolated` (3 sites)
  or `spawn_blocking` (5 sites, all on the wake_machine's private runtime).

### Q6 — Lock discipline on PR1 CRUD

For all 5 methods (db.rs:2962-3115): use `pool.get().await` + drop on
return; no struct-level Mutex/RwLock held across `.await`. Single-
statement transactions. **No findings.**

## Findings

### [R17-C1] `lessee_updated_at` never refreshed across phase transitions (CRITICAL)

- **File**: `crates/sandbox/src/db.rs:3015-3054` (`update_wake_job_state`)
  + `crates/sandbox/src/wake_machine.rs:480-493` (`set_state`).
- **Shape**: lessee column populated on INSERT (`admin_handlers.rs:1495`
  via server default) but no path refreshes it. Lease forever frozen.
- **Why critical**: PR1's a0888d9e commit message promises PR2 wires the
  takeover-sweep. Any sweep predicating on `lessee_updated_at < now() -
  threshold` (mirroring `sandboxes` takeover at `db.rs:2705-2706`) would
  reap every long-running wake at lease-expiry, mid-restore.
- **Action**: `update_wake_job_state` SQL gains `lessee_updated_at = now()`
  OR add a `renew_wake_job_lease(wake_id, lessee)` called from `set_state`.

### [R17-C2] Idempotency TOCTOU — find→insert is not atomic; no unique constraint (CRITICAL)

- **File**: `admin_handlers.rs:1457-1497` (`wake_sandbox_async_inner`) +
  `migrations/0009_wake_jobs.sql:106-108` (non-unique index).
- **Shape**: two concurrent POSTs both observe `Ok(None)` at
  `find_pending_wake_for_sandbox`, both call `insert_wake_job` (PK on
  `wake_id` succeeds for both), both `detach_isolated` a WakeMachine.
  One CAS wins at `update_sandbox_status(Restoring)`; the loser fails
  Internal at wake_machine.rs:246-252 → `Phase::Failed` → `rollback_and_classify`.
- **Downstream race**: the loser's `rollback_and_classify` (`:499-535`)
  calls `teardown_restore(vm_index)` → `release_vm_index(vm_index)`
  (`restore_handler.rs:2033`) — releasing the slot the WINNER just
  `reserve_vm_index`'d. **R10-C1 race shape via concurrent wake.**
- **Action**:
  - `CREATE UNIQUE INDEX wake_jobs_sandbox_pending_uniq ON
    sandbox.wake_jobs (sandbox_id) WHERE state NOT IN ('ok','failed')`,
  - `insert_wake_job` uses `INSERT ... ON CONFLICT DO NOTHING RETURNING`
    and on conflict re-queries `find_pending_wake_for_sandbox`.

### [R17-C3] Abandoned wake_jobs rows are GC-immortal; block idempotency forever (CRITICAL)

- **File**: `db.rs:3105-3115` (`gc_expired_wake_jobs`) + missing takeover
  sweep referenced in `db.rs:3096-3097` comment.
- **Shape**: GC predicate is `state IN ('ok','failed')`. Non-terminal
  rows excluded by design; the design assumes "PR2's takeover scan handles
  them". PR2 does not ship that scan.
- **Trigger**: controller crash mid-wake, OS-thread death,
  spawn_blocking panic that escapes the catch (line 309, 349, 368),
  process kill.
- **Downstream**: `find_pending_wake_for_sandbox` (`db.rs:3061-3091`)
  matches non-terminal rows FIRST. Every fresh wake POST for that
  sandbox returns 202 with the orphan + `replay: true`. Polling client
  sees the last-set in-flight state forever. **Permanent idempotency
  lock**; only escape is DBA-driven `DELETE FROM sandbox.wake_jobs`.
- **Action**: ship the wake_jobs takeover sweep in PR2 (or block PR2 on
  it). Predicate mirrors `db.rs:2705-2706`: `lessee_updated_at < now() -
  threshold AND state NOT IN ('ok','failed')`. Sweep action: roll back
  sandbox row + mark wake_jobs row `failed` with
  `WakeErrorCode::Internal`.

### [R17-I1] Wake-failure rollback can release the wrong vm_index under concurrent wake (IMPORTANT)

- **File**: `wake_machine.rs:499-535` (`rollback_and_classify`) +
  `:541-572` (`rollback_with`).
- **Shape**: even without R17-C2, when the failure is pre-reserve (CAS
  lost at `update_sandbox_status → restoring`), the rollback still calls
  `teardown_restore` which unconditionally releases vm_index. The
  losing machine never reserved the slot — but happily releases it on
  behalf of the winner.
- **Action**: split rollback into pre-reserve (NO teardown) vs
  post-reserve (full teardown). Eliminated by R4-A2 LeasedVmSlot RAII.

### [R17-I2] register_restored failure path unstested; persist=None skips post-livez triad (IMPORTANT)

- **File**: `wake_machine.rs:383-438` + all 6 e2e tests use `persist: None`.
- **Shape**: `persist=None` branch at `:425-438` SKIPS unseal +
  clock_resync + register_restored. The
  `rollback_with(RegisterFailed)` path (`:420-424`) is unstested e2e;
  `StubRestoreBackend` has no `fail_register` flag.
- **Severity**: IMPORTANT because R17-C2's race goes through
  register_restored on the winner and the rollback-via-CAS-loss on the
  loser — but the test surface can't simulate.
- **Action**: add `fail_register: bool` + a test using `persist: Some(...)`
  with a test-only Persistence (seeder fixture seals a record).

### [R17-I3] `wait_for_livez` may share R16-I2 alternating-answer pathology (IMPORTANT)

- **File**: `wake_machine.rs:360-378` (caller); `wait_for_livez`
  implementation in the backend.
- **Shape**: `wait_for_livez` is a polling loop hopped to spawn_blocking.
  The R16-I2 alternating-answer leak case was found in
  `wait_for_agent_silent` (a SEPARATE function) but the structural
  pattern may apply.
- **Action**: separate audit of `wait_for_livez`'s pass/fail criterion.
  If consecutive-failure threshold, apply R16-I2 sliding-window fix.

### [R17-M1] `spawn_blocking` under `detach_isolated` not exercised by detach.rs unit tests (MINOR)

- **File**: `wake_machine.rs:305, 344, 364, 513, 554` (5 sites) vs
  `detach.rs:148-243` unit tests (5 tests, none use spawn_blocking).
- **Shape**: compio's `spawn_blocking` uses the current-thread runtime
  context. `block_on` should establish context for the future poll
  duration. Likely safe but production-only path.
- **Action**: add one unit test in `detach.rs` driving `spawn_blocking`
  from inside `detach_isolated`. Test gap only.

### [R17-M2] Lessee value plumbed into WakeMachine struct but never read (MINOR / informational)

- **File**: `admin_handlers.rs:1481, 1502` + `wake_machine.rs:80`.
- **Shape**: `WakeMachine.lessee` field is set but never referenced in
  `WakeMachine::run`. Half-implemented discipline (column populated on
  INSERT, never refreshed per R17-C1, never checked per R17-C3).
- **Action**: hold for the PR shipping the takeover sweep. If
  intentionally deferred, document in PR2 commit message.

## Carry-forward audit

- **R4-A2** LeasedVmSlot RAII (14 cycles) — OPEN; R17-I1 dissolves under correct RAII.
- **R16-I1** sweep+registry sibling-C-6 — CLOSED at `96fa5f0f`.
- **R16-I2** fence-LEAK (1 cycle) — OPEN (phase tracing partial at `417cd6cd`); R17-I3 extends.
- **R16-M1** SealedAuth Zeroize (1) — OPEN; `wake_machine.rs:403` STILL passes `&sealed.signing_key_bytes` across `clock_resync.await`.
- **R16-M2** HA takeover scan multi-await (1) — OPEN.
- **R11-C1/C2** (6) — OPEN.

## Cross-lens consensus

- **R17-C1 + R17-C3 coupled**: both gated on the missing takeover sweep.
- **R17-C2 independent + lowest-cost**: UNIQUE INDEX in still-mutable
  migration 0009 + ON CONFLICT in `insert_wake_job`.
- **C-7-LT-PR2 PARTIAL** — "machine waits as long as needed" met by
  `detach_isolated`; "durable across controller restart" NOT met.
- **R4-A2** dissolves R17-I1 + R10-C1 + R11-C1 in one go.

## Lens hand-off

- **Architecture r17**: confirm R17-A5. Triage R17-C1+C2+C3 against
  the C-7-LT proposal scope.
- **Test-coverage r17**: R16-T5 emergency hold closed; R17-I2 is new gap.
- **Security r17**: R16-M1 SealedAuth Zeroize OPEN; wake_machine has a
  second cross-await use at line 403.
- **API-surface r17**: poll wire shape pinned by 6 unit tests
  (admin_handlers.rs:2222-2407). §10.0 envelope verified.

## PR2-FOLLOWUP concurrency gates

1. **[GATE-C1]** `update_wake_job_state` MUST set `lessee_updated_at =
   now()` (or a dedicated `renew_wake_job_lease` MUST be called from
   `WakeMachine::set_state`). R17-C1.
2. **[GATE-C2]** Migration 0009 MUST add UNIQUE INDEX on
   `(sandbox_id) WHERE state NOT IN ('ok','failed')`, AND
   `insert_wake_job` MUST handle the unique-violation by returning the
   existing wake_id. R17-C2.
3. **[GATE-C3]** Wake_jobs takeover sweep MUST land before PR2 reaches
   production. Predicate: `lessee_updated_at < now() - threshold AND
   state NOT IN ('ok','failed')`. Action: rollback sandbox row + mark
   wake_jobs row failed with `WakeErrorCode::Internal`. R17-C3.
4. **[GATE-I1]** `rollback_and_classify` MUST NOT call `teardown_restore`
   for pre-reserve failures (CAS-lost at restoring CAS). Split into
   pre/post-reserve rollback paths. R17-I1.
5. **[GATE-I2]** `StubRestoreBackend.fail_register` flag +
   `wake_machine_classifies_register_failure` test with `persist: Some(...)`.
   R17-I2.

## Status block

```
Round 17 (PR2 partial — 4 of 7 commits):
  NEW: R17-C1 (lessee_updated_at NEVER refreshed by update_wake_job_state;
       lessee discipline structurally absent in PR2),
   R17-C2 (idempotency TOCTOU find→insert; no UNIQUE INDEX on sandbox_id
       partial; concurrent POSTs spawn 2 machines; loser-rollback
       teardown_restore releases winner's vm_index — R10-C1 shape),
   R17-C3 (gc_expired_wake_jobs excludes non-terminal; promised takeover
       sweep ABSENT; orphan rows block idempotency forever),
   R17-I1 (rollback releases vm_index unconditionally on pre-reserve
       failure; dissolves under R4-A2),
   R17-I2 (register_restored failure path unstested; persist=None
       skips post-livez triad in all six new tests),
   R17-I3 (wait_for_livez may share R16-I2 alternating-answer pathology),
   R17-M1 (spawn_blocking under detach_isolated not exercised by
       detach.rs tests),
   R17-M2 (lessee plumbed but never read; informational).

  CARRY: R4-A2 (14 cycles), R14-V1, R15-D1, R16-I2, R16-M1, R16-M2,
       R11-C1/C2 OPEN; R16-I1 CLOSED at 96fa5f0f.

  ASK: (1) GATE-C1 bump lessee on every transition.
       (2) GATE-C2 UNIQUE INDEX + ON CONFLICT.
       (3) GATE-C3 ship takeover sweep before PR2 prod.
       (4) GATE-I1 rollback split pre/post-reserve.
       (5) GATE-I2 fail_register test gap.
       (6) Prioritize R4-A2 (14 cycles).
```
