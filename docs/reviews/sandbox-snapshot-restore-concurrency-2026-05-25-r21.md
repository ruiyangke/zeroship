# Sandbox/snapshot-restore — concurrency r21 review

Date: 2026-05-25 (UTC).
HEAD at audit: `4d73a5d1` (branch `feat/sandbox-snapshot-restore`).
Round 21 of N. READ-ONLY.

Scope: r17-Q3 cascade (`17d65f83` — `wake_job_row_from_pg` now
fallible), driver pins v7/v8 + sandbox-side `user_id` emission
(`03d2f4a8`, `4d73a5d1` — driver-only, concurrency-neutral), and
re-verification of r19/r20 carries against the live tree.

Prior: `…concurrency-2026-05-25-r20.md`.

## Summary

- **5 findings** (0 new CRITICAL, 1 new IMPORTANT, 3 MINOR/track,
  1 verification-pin). r17-Q3 cascade audited end-to-end — Err
  propagation clean, no panic exposed. **R20-C1 still OPEN** (no
  CAS guard added on `update_wake_job_state`); remains dominant
  race. **R20-I1, R20-I2, R20-I3 also still OPEN** — no commits
  since r20 touch them.
- **r17-Q3 closure verdict**: CLOSED on its own terms. All four
  production call paths handle `DatabaseError::DataIntegrity`
  correctly: two HTTP handlers funnel into `err_safe(500,
  "database_failed", …)`; the one internal caller
  (`insert_wake_job` retry loop) uses `?` to propagate, which
  short-circuits the 3-attempt loop and bubbles to the same
  handler. Takeover sweep and GC sweep do NOT route through
  `wake_job_row_from_pg`, so the new fallibility cannot reach them.
- **Restoring → LivezPolling crash-window** (the prompt's specific
  example): under the default 60 s threshold + 60 s sweep cadence,
  a wake whose Restoring phase clears 60 s is at risk every tick.
  Restoring is the single longest phase, has NO intermediate
  `lessee_updated_at` bump, and `MIN_TAKEOVER_THRESHOLD_SECS = 30`
  would make this worse if tuned down. See **R21-I1**.
- **R19-I4 retry × takeover sweep**: fresh INSERTs land with
  `lessee_updated_at = now()` (migration 0009:51 DEFAULT). The
  sweep cannot match a row younger than the threshold. **No new
  race.** PASS.

## Per-prompt-question audit

### Q1 — r17-Q3 cascade: DataIntegrity propagation

| Caller | Site | Handling | Verdict |
|---|---|---|---|
| `Database::get_wake_job` | `db.rs:3167` | `.transpose()` returns `Result<Option<WakeJobRow>>` | clean |
| `Database::find_pending_wake_for_sandbox` | `db.rs:3266` | same `.transpose()` shape | clean |
| `poll_wake` HTTP handler | `admin_handlers.rs:1778-1797` | `Err(e) => err_safe(500, "database_failed", …)` | clean — surfaces as 500 |
| `wake_sandbox_async` idempotency precheck | `admin_handlers.rs:1565-1586` | `Err(e) => err_safe(500, "database_failed", …)` | clean |
| `Database::insert_wake_job` retry loop | `db.rs:3118-3121` | `match …await?` — `?` propagates DataIntegrity, short-circuits 3-attempt loop | clean |

**No exhaustive `match` on `DatabaseError` exists** (grep confirms
only `CasLost` and `NotFound` are pattern-matched). The new variant
is back-compat at the type level. **No `.unwrap()` / `.expect()` in
production callers** on the new fallible signature; only test code
calls `.unwrap()`, and those consume `Result<Option<WakeJobRow>>`
*after* the new Err path. **Production panic surface: none.** PASS.

The sweep (`run_wake_jobs_takeover_once`) decodes its own `RETURNING
wake_id` via `rows.len()` (`db.rs:3373`); it never calls
`wake_job_row_from_pg`. GC sweep (`gc_expired_wake_jobs`) executes a
DELETE returning `u64`. Neither can land in DataIntegrity. PASS.

### Q2 — R19-C1 takeover threshold vs. wake-phase wall time

Wake phases and `lessee_updated_at` bumps (via
`update_wake_job_state`'s unconditional `lessee_updated_at = now()`
at `db.rs:3214`):

| Phase | Entry | Work to next entry |
|---|---|---|
| `Pending` | `db.rs:3091` (migration default) | sub-ms |
| `ReservingSlot` | `wake_machine.rs:244` | `reserve_vm_index_with_retry` — observed up to ~32 s at 17 attempts × 2 s |
| `Restoring` | `wake_machine.rs:273` | **alloc_dir teardown + sync `store.get` (GCS read on multi-GB blob + AEAD decrypt) + `rewrite_config_json` + `submit_restore_job`** — easily 30-120 s cold-cache |
| `LivezPolling` | `wake_machine.rs:365` | `wait_for_livez` (R19-I1 two-phase, up to `agent_livez_timeout_secs` ≈ 30 s) |
| `ClockResyncing` | `wake_machine.rs:388` | sub-second |
| `Registering` | `wake_machine.rs:420/438` | sub-second |

**Crash between `Restoring` and `LivezPolling`** (the prompt's
specific scenario): the row has `state=restoring` and
`lessee_updated_at` frozen at Restoring-entry time. Once aged past
`takeover_threshold_secs` (default 60 s, floor 30 s), the sweep
claims → `(failed, wake_worker_aborted)`. **The threshold is too
tight against a worst-case healthy wake.** Sweep cadence
(`WAKE_JOBS_TAKEOVER_POLL_SECS = 60`, `sweep.rs:367`) equals the
default threshold — an unstable equilibrium. r17 cluster smoke
shows a wake hitting terminal at +49.86 s in Restoring (driver
rewriter failure, not takeover); a healthy 1+ GB cold-cache restore
could clear 60 s.

This is **R20-I3 re-confirmed and unaddressed**. See **R21-I1**.

### Q3 — R19-I4 retry loop × R19-C1 takeover sweep interaction

The retry loop (`db.rs:3087-3138`) INSERTs against the partial
UNIQUE INDEX `wake_jobs_sandbox_pending_uniq`. INSERT either:

1. Lands → `Inserted`. Fresh row's `lessee_updated_at = NOW()`
   (migration 0009:51 DEFAULT). **Sweep cannot match a 0 ms-old
   row.** Safe.
2. Conflicts → SELECT via `find_pending_wake_for_sandbox`. If sweep
   transitions winner to terminal between conflict and SELECT,
   SELECT returns None (filter `state NOT IN ('ok','failed')`),
   retry. Fresh INSERT now succeeds. Outcome: caller B gets a fresh
   wake_id. Intended R19-C1 behaviour; matches r20 Q2.b/Q2.c. **No
   new race.** PASS.

Sandbox row may still be `Restoring` at this point — caller B's
pre-flight check (`admin_handlers.rs:1614-1631`) 409s until
transient-state sweep rolls back. This is **R20-M1 carried**.

### Q4 — Carry verification against live tree

| Finding | r20 | r21 | Evidence |
|---|---|---|---|
| **R19-C1** wedge-half | CLOSED | CLOSED carry | `db.rs:3300-3374`, `sweep.rs:388-464` unchanged |
| **R20-C1** CAS guard on `update_wake_job_state` | NEW-CRITICAL | **STILL OPEN** | `db.rs:3207-3217` SQL has only `WHERE wake_id = $5` |
| **R19-I1** livez two-phase | CLOSED | CLOSED carry | r20 verdict stands |
| **R19-I4** insert retry | CLOSED | CLOSED carry | `db.rs:3079-3145` present |
| **R20-I1** `wake-takeover` enumeration | NEW-IMP | **STILL OPEN** | `detach.rs:234-250` STATIC_NAMES lacks `"wake-takeover"` |
| **R20-I2** sweep host-scoping | NEW-IMP | **STILL OPEN** | `db.rs:3354-3372` UPDATE has no `AND lessee = $host` |
| **R20-I3** Restoring wall-time | NEW-IMP | **STILL OPEN** | `wake_machine.rs:273-365` no intermediate `set_state`; `config.rs:972` `MIN_TAKEOVER_THRESHOLD_SECS = 30` unchanged. Restated as **R21-I1** |
| R20-M1/M2/M3/M4/M5, R19-I2/I3 | carry | carry | unchanged |
| **r17-Q3** | — | **CLOSED** (`17d65f83`) | `db.rs:1625-1657`; see Q1 |

## Findings

### [R21-I1] Restoring-phase wall-time crash-window — watchdog recommendation (re-state of R20-I3)

- **Files**: `wake_machine.rs:273-365`, `db.rs:3300-3374`,
  `config.rs:972`, `sweep.rs:367`.
- **Shape**: `set_state(Restoring)` at line 273 is followed by
  alloc_dir teardown + sync `store.get` (GCS read + AEAD decrypt
  + write multi-GB blob) + `rewrite_config_json` +
  `submit_restore_job`. None issue intermediate
  `update_wake_job_state` — `lessee_updated_at` stays frozen at
  Restoring-entry time. With default 60 s threshold + 60 s sweep
  cadence, a Restoring wall-time of 60-120 s is at risk every tick.
  No mechanism exists to distinguish "slow but alive" from
  "crashed" within Restoring — pure lease-renewal lacuna.
- **Cascade with R20-C1**: with R20-C1 unfixed, takeover-claim
  directly clobbers a live machine. With R20-C1 fixed (CAS guard),
  the machine's later writes become no-ops but the pg row reads
  `failed/wake_worker_aborted` for the rest of the actual successful
  wake — operator-dashboard contradiction (sandbox eventually
  Running while wake_jobs row stays failed) and poll-client UX trap.
- **Action**: two options (recommend 1):
  1. Watchdog inside `Restoring`: spawn a 20-30 s tick calling
     `update_wake_job_state(Restoring, None, None, None)` purely
     to bump `lessee_updated_at`. Cheap (one indexed UPDATE/tick).
  2. Raise floor + default: `MIN_TAKEOVER_THRESHOLD_SECS = 60`,
     `DEFAULT_TAKEOVER_THRESHOLD_SECS = 180`. Cheaper but slower
     real-crash recovery. Also: split sweep cadence from threshold
     (e.g., cadence 30 s, threshold 180 s — asymmetric defaults
     give sweep multiple chances to observe a healthy bump).

### [R21-V1] r17-Q3 cascade verified — no panic surface, no missing handler

- **Files**: `db.rs:1625-1657`, `db.rs:3149-3168`, `db.rs:3239-3267`,
  `db.rs:3118-3121`, `admin_handlers.rs:1565,1778`.
- **Shape**: r17-Q3 promoted `wake_job_row_from_pg` to fallible
  (returns `Err(DatabaseError::DataIntegrity)` on unknown state).
  Audit table in Q1 traces every production caller; none panic.
  Every site funnels into `err_safe(500, "database_failed", …)`
  or `?` propagation to the same handler.
- **Verdict**: PASS. Pin only — the closure does not ship a pg-
  gated round-trip test that deliberately writes a CHECK-bypass
  row (it pins the precondition only). Hand off to test-coverage.

## MINOR

### [R21-M1] r17-Q3 widens `?`-propagation surface in insert_wake_job retry loop — no pool-hold widening

- **File**: `db.rs:3087-3138`.
- **Shape**: before r17-Q3, `find_pending_wake_for_sandbox` could
  only fail with pg errors (which already `?`-propagated). After
  r17-Q3, the same call can ALSO `?`-propagate `DataIntegrity`.
  The loop holds a single `pool.get()` client (line 3085); the
  inner call opens a separate client (db.rs:3244) which is
  released before the DataIntegrity decoder fires. **No pool-hold
  widening.** Tracking pin only — pairs with R20-M2.

### [R21-M2] migration 0009 CHECK constraint is load-bearing for DataIntegrity unreachability

- **File**: `crates/sandbox/migrations/0009_wake_jobs.sql:58-79`.
- **Shape**: production-unreachability of `DataIntegrity` is
  predicated on `wake_jobs_state_check`. A future migration that
  drops or rewrites the constraint without preserving the
  discriminator set voids the Q1 panic-free guarantee — every
  handler's `err_safe(500)` becomes the sole defense. Migration
  0012 adds `wake_worker_aborted` to `error_code` CHECK
  (`migrations/0012_wake_jobs_aborted_code.sql:61`) but does not
  touch the state constraint.
- **Verdict**: hand off to architecture-r21 — encode the invariant
  in an ADR alongside migration 0012.

### [R21-M3] sweep cadence equals threshold default — unstable equilibrium

- **File**: `sweep.rs:367` (`WAKE_JOBS_TAKEOVER_POLL_SECS = 60`),
  `config.rs:1029` (`DEFAULT_TAKEOVER_THRESHOLD_SECS = 60`).
- **Shape**: cadence == threshold means any wake whose Restoring
  wall-time hovers around the threshold IS claimed in exactly one
  sweep tick. Asymmetric defaults would give the sweep multiple
  ticks to observe a legitimate `lessee_updated_at` advance before
  claiming. Pairs with R21-I1 recommendation.
- **Verdict**: arch-r21 tracking pin.

## Cross-lens consensus

- **R20-C1** remains the dominant unfixed race and gating finding
  for the next implementation pass. One-line SQL fix.
- **R21-I1** (= R20-I3 re-stated) is the dominant operator-UX
  concern even after R20-C1 lands — a successful wake whose
  Restoring clears 60 s leaves a ghost `failed` row contradicting
  the eventual `Running` sandbox status. Watchdog tick is the
  cleanest fix.
- **R20-I1** (detach.rs enumeration) is a hygiene gap, still
  one-line.
- **r17-Q3 cascade** is clean — no callers regress; no new races
  introduced.

## Lens hand-off

- **Architecture r21**: R20-I2 (cross-controller claim semantics).
  R21-M2 (CHECK-constraint ADR). R21-M3 (cadence vs threshold
  symmetry).
- **Test-coverage r21**: R20-C1 (controller-still-alive +
  takeover-claims-mid-flight). R21-V1 (pg-gated round-trip for
  DataIntegrity via deliberately bypassed CHECK — optional).
- **Code-quality r21**: R20-I1 enumeration miss.
- **Security r21**: R20-M5 carry.

## Status block

```
Round 21 (r17-Q3 + driver pin v7/v8 LANDED):
  CLOSED:
    r17-Q3 (17d65f83 — wake_job_row_from_pg returns Err; no
      production panic surface).
  CLOSED carry from r20:
    R19-C1 wedge-half, R19-I1, R19-I4.
  STILL OPEN from r20 (no commits since r20 touch these):
    R20-C1 (terminal write CAS guard — CRITICAL),
    R20-I1 (wake-takeover STATIC_NAMES — IMPORTANT),
    R20-I2 (sweep host-scoping — IMPORTANT),
    R20-I3 (Restoring wall-time vs. threshold — IMPORTANT;
      re-stated as R21-I1).
  CARRY:
    R19-I2, R19-I3, R20-M1, R20-M2, R20-M3, R20-M4, R20-M5.
  NEW (r21):
    R21-I1 (Restoring-phase watchdog re-statement),
    R21-V1 (r17-Q3 audit — PASS pin),
    R21-M1 (insert_wake_job retry pool-hold unchanged),
    R21-M2 (migration 0009 CHECK invariant),
    R21-M3 (sweep cadence == threshold default).

  ASK: (1) GATE-C4 — add CAS guard to update_wake_job_state
       (R20-C1); pre-req for clean hand-off;
       (2) GATE-I3 — Restoring watchdog OR asymmetric defaults
       (R21-I1);
       (3) hygiene PR for R20-I1;
       (4) arch-r21 decide R20-I2.
```
