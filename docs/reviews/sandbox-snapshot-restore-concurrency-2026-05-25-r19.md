# Sandbox/snapshot-restore — concurrency r19 review

Date: 2026-05-25 (UTC).
HEAD at audit: `b8654600` (branch `feat/sandbox-snapshot-restore`).
Round 19 of N. READ-ONLY.
Scope: C-7-LT-2-PR1+PR2 (`40811d8b`, `bfff5acc`), R17-S1 (`3c75a8ce`),
R18-I1 (`531db5c3`), visibility-tightening (`f27062c0`).

Prior: `…concurrency-2026-05-25-r18.md`. Cross-lens: arch-r19.

## Summary

- **9 findings** (1 CRITICAL, 4 IMPORTANT, 4 MINOR). R18-C1 (GATE-C2)
  LANDED at `db248cbf` + migrations/0011; R18-M1 LANDED at `3c75a8ce`.
- **C-7-LT-2 race surfaces clean** under the compio-native probe.
  Deadline-vs-in-flight-probe overshoots by ≤ 150 ms; alternating-
  answer pathology is structurally impossible (`accept()` Ok-vs-Err
  is the only classifier now).
- **R18-A1 (wake_jobs takeover sweep) is now load-bearing** under
  GATE-C2's UNIQUE INDEX. Stale non-terminal row → every subsequent
  wake POST for that sandbox is permanently blocked. **R19-C1**
  (escalates arch-r19-A2 from concurrency lens).
- **`wait_for_agent_livez` (startup, nomad_ch.rs:3066) carries the
  same ureq wedge shape** the teardown probe just fixed. Bounded by
  CreateGuard::drop tear-down so IMPORTANT, not CRITICAL. **R19-I1.**
- **Lessee CAS continuity verified**: `update_wake_job_state` UPDATE
  never touches the `lessee` column; only `lessee_updated_at = now()`.
  Original lessee preserved across all retry/replay paths.

## Per-prompt-question audit

### Q1 — `wait_for_agent_silent` deadline races

Loop (nomad_ch.rs:3272-3353):
```
if Instant::now() >= deadline { break; }       // (1)
let reachable = probe(addr, 150ms).await;       // (2)
if consecutive_misses >= 2 { return Ok(()); }   // (3)
sleep(100ms - probe_elapsed).await;              // (4)
```

**Deadline-vs-probe** (Q1.a): check at (1) is loop-top only. Once past
(1), the in-flight probe at (2) runs to its own 150 ms
`compio::time::timeout`; outer `host_fence_timeout` does NOT cancel it.
Worst case: one in-flight probe at deadline → fence overshoots by
≤ 150 ms. **Not a correctness issue** (consecutive misses is the
contract, not wall-time); see R19-M1.

**Probe respects outer deadline?** (Q1.b): NO. The probe always runs
to its 150 ms ceiling. Acceptable because 150 ms is 0.5% of the 30 s
budget. Pathological only if operator sets `host_fence_timeout_secs
< 5` (and `= 0` has its own bypass at nomad_ch.rs:1089).

**Alternating-answer (Q1.c, R16-I2 concern)**: structurally impossible.
`accept()` Ok ⇒ socket alive regardless of post-accept HTTP. The
previous "HTTP 200 then 5xx flips counter" pathology is gone. Pinned
by `host_fence_pr1_alternating_accept_drop_times_out_with_misses_zero`
(nomad_ch.rs:5025-5048). **PASS.**

### Q2 — R18-A1 takeover sweep absence (escalated)

`update_wake_job_state` writes `lessee_updated_at = now()` every
transition (db.rs:3128) — confirmed at r18. But **nothing reads
`wake_jobs.lessee_updated_at`**. Only `gc_expired_wake_jobs`
(db.rs:3194-3211) touches the table, filtered to terminal states
only:

```sql
DELETE FROM sandbox.wake_jobs
  WHERE state IN ('ok','failed')
    AND updated_at < now() - make_interval(secs => $1::BIGINT)
```

Non-terminal stuck rows (controller-crash mid-wake) **never clean up**.
With GATE-C2's `wake_jobs_sandbox_pending_uniq` (migrations/0011),
every subsequent wake POST for that sandbox returns
`InsertWakeJobOutcome::Replay(stale_row)`; the handler short-circuits
at admin_handlers.rs:1675-1686 with the dead wake_id. **Permanent
block per sandbox.** See **R19-C1**.

### Q3 — Sibling site audit: `wait_for_agent_livez`

`nomad_ch.rs:3082-3091` uses the same ureq+spawn_blocking shape as
the pre-fix teardown probe:

```rust
let livez_status = compio::runtime::spawn_blocking(move || {
    ureq::get(&probe_url)
        .timeout(Duration::from_millis(500))   // request-deadline
        .call()
        ...
}).await.ok().flatten();
```

The wedge IS possible in the startup direction: half-collapsed routes
(stale-tenant CH up but new TAP not fully wired) can hang SYN for
~30 s. Each stuck probe burns ~30 s vs. the 150 ms design cadence.

**Why not CRITICAL**: bounded by (a) `agent_livez_timeout_secs`
outer budget at nomad_ch.rs:801 and (b) CreateGuard::drop firing
`detach_isolated create-rollbk` (R17-A5, nomad_ch.rs:2008), which
tears down partial state and releases vm_index via the fixed
teardown probe. **No new leak shape**, but observable 30 s+ create
latency under stress. See **R19-I1**.

### Q4 — vm_index leak counter race

`metrics::inc_vm_index_leak` (metrics.rs:252-269) uses
`AtomicU64::fetch_add(_, Relaxed)`. Three call contexts (ntex worker,
`snap-idle-gc` detach, `create-rollbk` detach). Relaxed is
cross-runtime-safe for monotonic counters; no torn `load`. **No
race.** Counter-bump and log-emit aren't transactional — observability
gap, R19-M3.

### Q5 — Lessee CAS continuity

`update_wake_job_state` SQL (db.rs:3121-3131) SET list does NOT
include `lessee`. Only `state`, COALESCE'd errors, `updated_at`,
`lessee_updated_at`, optionally `ready_at`. Lessee retains the value
set at `insert_wake_job` (db.rs:3022) unchanged across every
transition. `set_state` passes `None, None, None` for the COALESCE'd
fields — preserved across retries. Best-effort intermediate writes
(wake_machine.rs:493-498) mean pg blips don't break continuity.
**R17-A1 wired correctly.**

## Findings

### [R19-C1] Stale wake_jobs + GATE-C2 UNIQUE INDEX = permanent wake block

- **Files**: `sweep.rs:288-321`, `db.rs:3194-3211` (GC filters
  terminal only); `migrations/0011_wake_jobs_unique.sql:30` (UNIQUE
  INDEX); `admin_handlers.rs:1675-1686` (replay short-circuit).
- **Shape**: controller crash / panic / OOM mid-wake leaves the row in
  a non-terminal state. `gc_expired_wake_jobs` skips it. With
  migration 0011's UNIQUE INDEX every subsequent wake POST for the
  same sandbox returns `InsertWakeJobOutcome::Replay(stale_row)`;
  handler short-circuits with a 202 pointing at the dead wake_id.
  Permanent block per sandbox; persists across controller restarts
  (durable in pg).
- **Why concurrency-lens**: `lessee_updated_at` is bumped on every
  transition by design (R17-A1) so a sweep CAN distinguish abandoned
  from in-flight rows by lease age. **The lease-renewal write has no
  reader.**
- **Trigger probability**: low per-sandbox per controller-lifetime,
  but UNBOUNDED accumulation (once stuck, blocks every retry forever).
- **Action**: extend `gc_expired_wake_jobs` (or add
  `gc_stale_in_flight_wake_jobs`) with:
  ```sql
  UPDATE sandbox.wake_jobs
     SET state = 'failed',
         error_code = 'internal',
         error_message = 'controller crash before terminal write',
         updated_at = now(),
         lessee_updated_at = now()
   WHERE state NOT IN ('ok','failed')
     AND lessee_updated_at < now() - make_interval(secs => $1::BIGINT)
  ```
  Wire into `spawn_wake_jobs_gc` loop (sweep.rs:331). Threshold ≥ max
  wake-ladder wall-time + safety margin (suggest 600 s).

### [R19-I1] `wait_for_agent_livez` startup probe carries the ureq wedge

- **File**: `nomad_ch.rs:3082-3091`.
- **Shape**: identical ureq+spawn_blocking pattern; `.timeout(500ms)`
  is request-deadline. Half-collapsed routes at create-time can hang
  SYN ~30 s per probe.
- **Why IMPORTANT not CRITICAL**: blast radius bounded by outer
  `agent_livez_timeout_secs` AND `CreateGuard::drop` tearing down
  partial state through the fixed teardown probe. Costs latency, not
  slot leaks.
- **Action**: mirror C-7-LT-2-PR1 — replace ureq probe with
  `probe_agent_reachable_tcp` as a cheap gate before the signed
  /version step. The /version call still needs HTTP (signed pubkey
  check), but the connect-only gate eliminates the wedge surface.

### [R19-I2] R18-I2 rollback unconditional teardown — CARRY (defense-in-depth)

- **File**: `wake_machine.rs:506-579` (rollback_and_classify,
  rollback_with both unconditionally `teardown_restore`).
- **Shape**: post-GATE-C2 the reachable race shape is closed — UNIQUE
  INDEX prevents two WakeMachines on one sandbox, so a loser can't
  release the winner's slot. **Still unsafe in shape.** A future
  allocator bug producing parallel WakeMachines on different
  sandbox_ids sharing a vm_index would re-open it.
- **Action**: lower-priority; R4-A2 (LeasedVmSlot RAII) dissolves
  long-term.

### [R19-I3] R18-I3 register_restored fail-path untested — CARRY

- **File**: `tests/sandbox_pg_e2e.rs::wake_machine_e2e`.
- **Shape**: all 6 e2e tests use `persist: None`, short-circuiting
  the unseal+resync+register triad. `rollback_with(RegisterFailed)`
  has zero coverage.
- **Action**: r20 GATE-I2 — `fail_register: bool` on
  `StubRestoreBackend` + test with a seeder Persistence.

### [R19-I4] `insert_wake_job` ON CONFLICT → terminal race → 500

- **File**: `db.rs:3015-3058`.
- **Shape**: ON CONFLICT path reads `find_pending_wake_for_sandbox`
  AFTER the conflict. If the winner's WakeMachine transitions to
  terminal between PG's conflict-resolution and the follow-up
  SELECT (sub-ms window), the SELECT returns None and the caller
  returns `DatabaseError::Validation` → 500 at the handler.
- **Practical likelihood**: tiny (state transitions take ≫ 1 ms).
  Observable in fast-stub test fixtures. Failure mode: caller sees
  500 without recourse, but a re-issued POST would now succeed
  because terminal rows don't block the UNIQUE INDEX.
- **Action**: on the "no pending row" branch, re-attempt the
  `INSERT … ON CONFLICT DO NOTHING` (terminal partition is no longer
  blocking). One extra DB round-trip on a sub-ms race window;
  bounded retry.

## MINOR

### [R19-M1] `wait_for_agent_silent` outer deadline overshoot ≤ 150 ms

- **File**: `nomad_ch.rs:3272-3353`.
- **Shape**: see Q1. Loop-top deadline check; in-flight probe runs
  to its own 150 ms ceiling.
- **Action**: optional — `probe_agent_reachable_tcp(addr,
  min(CONNECT_TIMEOUT, deadline - Instant::now()))`. One line.
  Not actionable unless operator sets `host_fence_timeout_secs < 5`.

### [R19-M2] Sanitizer hostname-trail post-truncation

- **File**: `wake_machine.rs:717-735`.
- **Shape**: 256-byte truncation runs AFTER redaction. A long error
  body with a redaction-evading hostname segment
  (e.g. `agent.internal.cluster.local`) survives intact if the
  truncation slices elsewhere. Risk low — wake-path
  `tracing::warn!` at wake_machine.rs:154-160 already logs
  unredacted to operator-only journald; durable column is sanitized.
- **Action**: hostname allowlist (zeroship.{ai,io,dev}) or strip
  between `:` and `/` post-truncation.

### [R19-M3] Cross-runtime counter / log correlation

- **File**: `metrics.rs:252-282`. Counter bump + tracing emit not
  transactional; observability gap not a concurrency bug.

### [R19-M4] R16-M1 SealedAuth Zeroize cross-await — CARRY

- **File**: `wake_machine.rs:410` — `&sealed.signing_key_bytes` borrow
  across `clock_resync.await` (407-412). Activates only when
  `SealedAuth` impls ZeroizeOnDrop. Tracking pin only.

## Cross-lens consensus

- **R19-C1** is the highest-priority concurrency gap remaining.
  GATE-C2 + C-7-LT-2 closing eliminated the dominant slot-leak
  shapes; the remaining shape is **wake-block per stale row**, not
  vm_index loss. Arch-r19-A2 raises this from the architecture lens;
  this review escalates from the concurrency lens citing the
  lessee_updated_at-without-reader asymmetry.
- **R19-I1** is shared with test-coverage (startup-probe wedge stress).
- **R19-I2 carry** is structurally unreachable post-GATE-C2 — still
  a defense-in-depth gap.
- **R19-M3** crosses observability — not blocking PR2 ship.

## Lens hand-off

- **Architecture r20**: R19-C1 fixer design — extend
  `gc_expired_wake_jobs` (concurrency-lens preference: one loop,
  one cadence) vs. separate takeover sweep (arch-r19-A2 preference).
- **Test-coverage r19**: (1) R19-I3 fail_register; (2) R19-I1
  startup-probe wedge repro via black-hole listener; (3) R19-C1
  controller-crash-mid-wake test driving the permanent block.
- **Security r19**: R19-M2 sanitizer hostname-trail.
- **API-surface r19**: R19-I4 wake-POST 500-on-race — typed retry
  envelope or silent retry?

## Status block

```
Round 19 (C-7-LT-2 + R17-S1 + R18-I1 LANDED):
  CLOSED:
    R18-C1 (GATE-C2 UNIQUE INDEX — db248cbf + migrations/0011),
    R18-M1 (sanitizer 169.254/16 + 100.64/10 — 3c75a8ce),
    R18-I1 (Inserted assertion fixtures — 531db5c3),
    R17-API1 + R10-API1 (visibility — f27062c0),
    C-7-LT-2 (probe wedge — 40811d8b + bfff5acc).
  CARRY:
    R18-I2 → R19-I2 (defense-in-depth post-GATE-C2),
    R18-I3 → R19-I3 (register_restored coverage),
    R18-M3 → R19-M4 (Zeroize cross-await),
    R18-M4 (sanitizer Latin-1 char cast).
  NEW:
    R19-C1 (stale wake_jobs + GATE-C2 = permanent block — CRITICAL),
    R19-I1 (wait_for_agent_livez startup ureq wedge),
    R19-I4 (insert_wake_job ON CONFLICT → terminal → 500),
    R19-M1 (silent-fence ≤ 150 ms overshoot),
    R19-M2 (sanitizer hostname-trail),
    R19-M3 (counter / log correlation).

  ASK: (1) r20 GATE-C3 wake_jobs takeover sweep (R19-C1) — escalated;
       (2) r20 mirror C-7-LT-2 fix into wait_for_agent_livez (R19-I1);
       (3) r20 GATE-I2 fail_register test (R19-I3);
       (4) decide R19-I4 retry-or-500 contract.
```
