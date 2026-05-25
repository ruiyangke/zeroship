# Sandbox/snapshot-restore — test-coverage r17 review

Date: 2026-05-25 (UTC). HEAD at audit: `163724dc`. Latest production-code
commit: `c2f24ede` "sandbox/tests: wake_machine pg-gated stub fixtures
(C-7-LT-PR2)". Round 17. Prior: `docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r16.md`.

## Summary

- 7 NEW findings (1 POSITIVE closing EMERGENCY, 3 IMPORTANT, 3 MINOR)
  + 5 carry-forwards re-filed.
- Sandbox lib tests **360 → 373 (+13)** across PR2's 4 commits.
  Per-commit walk:
  - `17e9f421` wake_machine state machine: **+5** (3 in
    `wake_machine.rs` `classify_failure`/`sandbox_id_typed`; +1 in
    `typed_id.rs` `wake_prefix_is_three_chars`; +1 in `metrics.rs`
    `wake_sync_deprecated_counter_monotonic`).
  - `98032273` dual-mode handler + poll: **+8** (6 `compio::test` against
    `render_wake_poll_response` + 2 `#[test]` on `WakeQuery`).
  - `9f006c87` sweep GC: **+0** lib tests (pg-gated coverage landed
    in the next commit).
  - `c2f24ede` integration fixtures: **+0 lib tests** but **+6
    `compio::test` integration tests** at `sandbox_pg_e2e.rs:3994-4396`.
- **R16-T1 EMERGENCY HOLD: CLOSED.** The 8-cycle declared-but-unused
  status on `StubRestoreBackend::fail_submit` + `::fail_livez` ends:
  both flags drive `WakeMachine::drive` against a pg-seeded sandbox
  row at `sandbox_pg_e2e.rs:4214` (submit) + `:4158` (livez). Rollback
  CAS to `Snapshotted` asserted at `:4193` + `:4239`; wire-code
  mapping asserted at `:4181`.
- **Integration directory moves off zero**: +751 LOC / +6 tests in
  `crates/sandbox/tests/sandbox_pg_e2e.rs` since `b8fae7b7`. The
  7-round flatline ends.

## Trend table (9-cycle history)

| Cycle | sandbox lib | sandbox-agent lib | Δ sandbox | Notes |
|-------|-------------|-------------------|-----------|-------|
| r9    | 296         | 231               | —         | baseline |
| r16   | 344         | 242               | +12       | C-8/A1 (orthogonal to wake) |
| **r17** | **373**   | 242 (assume flat) | **+29**   | **PR1 (+16 db.rs CRUD/enums) + PR2 (+13)** |
| r9→r17 | +77        | +11               | —         | **integration delta NON-ZERO this cycle** |

+29 is the largest single-cycle sandbox-lib delta in audit history
(prior record r12 = +17), AND concentrated on the wedge surface for
the first time since r9.

## R16-T1 EMERGENCY HOLD verdict: **CLOSED**

Citations (all in `crates/sandbox/tests/sandbox_pg_e2e.rs`):

- `:4109-4143` `wake_machine_drives_snapshotted_to_ok`: happy path;
  terminal Ok; sandbox `Running`; `ready_at`/`agent_url` set.
- `:4145-4197` `wake_machine_classifies_livez_failure`: `fail_livez=true`,
  terminal `Failed` + `LivezTimeout`, `wire_code()=="livez_timeout"`,
  sandbox rolls back to `Snapshotted` (NOT suspect).
- `:4200-4244` `wake_machine_classifies_submit_failure`:
  `fail_submit=true`, terminal `Failed` + `RestoreFailed`, rollback.
- `:4247-4291` `wake_machine_classifies_reserve_failure`:
  `fail_reserve=true`, terminal `Failed` + `SlotUnavailable` (wire
  `vm_index_unavailable`).
- `:4296-4347` `wake_machine_terminal_clears_find_pending`:
  post-terminal `find_pending_wake_for_sandbox` returns None — pins
  the invariant the handler's idempotency branch relies on.
- `:4350-4395` `wake_machine_gc_sweep_evicts_terminal_rows`:
  `gc_expired_wake_jobs(Duration::from_secs(0))` deletes terminal row.

The four R15-T1 sub-asks from r16:

| Sub-ask | r16 | r17 |
|---------|-----|-----|
| #1 pg-e2e `fail_submit` end-to-end | DEAD | **CLOSED** (:4214) |
| #1' pg-e2e `fail_livez` end-to-end | DEAD | **CLOSED** (:4158) |
| #2 `persist=Some(_)` chain | DEAD | **STILL OPEN** — see R17-T2 |
| #3 future-drop wrap of `reserve_vm_index_with_retry` | UNTESTED | **PARTIAL** — `detach_isolated` spawn arguably moots; see R17-T6 |
| #4 OS-thread detach pattern unit test | ZERO | **PARTIAL** — tests call `machine.drive()` directly; see R17-T6 |

2 fully closed (the highest-value pair), 1 carry-forward, 2 partial/
architectural-mooted. EMERGENCY qualifier dropped.

## CRITICAL

(none new)

## IMPORTANT

### [R17-T1] PR2 integration fixtures close the R10-T1/T2/R16-T1 carry (POSITIVE)

The 6 new pg-gated tests are the audit's preferred shape: end-to-end
drive of the wedge surface, stub failure injection at every classified-
error branch, asserted row state in pg. Quality observations:

- `migrated_db_arc()` (`:4036`) correctly shares the `Database` Arc
  between seeder + WakeMachine so the `update_sandbox_status` CAS
  fences on the SAME host_id (inline comment explicitly names this
  trap).
- Each test calls `seed_snapshotted_row` — the existing fixture; no
  parallel infrastructure.
- Wire-code drift caught at TWO layers: lib test
  `wake_error_code_wire_code_uses_existing_envelope_codes`
  (`db.rs:3330-3361`) pins the table; integration test asserts
  `code.wire_code()` directly.

R10-T1/T2 + R16-T1 carry materially closed.

### [R17-T2] `persist=Some(_)` chain still uncovered post-PR2 (R10-T2 carry, 9th round)

- **Files**: `wake_machine.rs:392-422` (the `if let Some(p) = self.persist`
  branch — unseal/clock_resync/register_restored); `:425-435` (the
  test-fixture `else` branch — the one PR2's 6 tests all hit);
  `sandbox_pg_e2e.rs:4072` (`persist: None`).
- **Symptom**: all 6 PR2 integration tests construct `WakeMachine`
  with `persist: None`. `WakeErrorCode::{ClockResyncFailed, RegisterFailed}`
  are pinned by lib drift-guard tests but never DRIVEN by any
  integration test. 4-of-7 wire codes integration-tested; 3 remain
  drift-only.
- **Action**: 7th fixture wiring a real `Persistence`, stub-failing
  one of the persist-chain methods, asserting terminal `ClockResyncFailed`
  or `RegisterFailed`. ~80 LOC. CARRY to R18.

### [R17-T3] Concurrent re-POST race: `find_pending_wake_for_sandbox` → `insert_wake_job` TOCTOU window with no DB-level uniqueness guard; no test pins behaviour

- **Files**: `admin_handlers.rs:1540-1574` (idempotency lookup);
  `:1623-1648` (insert+spawn); `migrations/0009_wake_jobs.sql:46-92`
  (only `wake_id PRIMARY KEY`; `wake_jobs_sandbox_idx` is NOT unique;
  no partial UNIQUE on `(sandbox_id) WHERE state NOT IN ('ok','failed')`).
- **Symptom**: two requests POST `/wake` within the few ms between
  lookup at `:1545` and insert at `:1644`. Both see `find_pending →
  None`, both mint a fresh `wake_id`, both `insert_wake_job` succeeds
  (PRIMARY KEY is `wake_id`, not `sandbox_id`), both spawn
  `WakeMachine`. The CAS `update_sandbox_status(Snapshotted →
  Restoring, gen=g0)` is generation-fenced so one wins; the loser
  returns `Internal` ("CAS failed") and rolls back — BUT its
  `wake_id` row sits in `Pending → Internal` until GC. A polling
  client given the loser's `wake_id` sees pending → Internal failure.
- **Why it matters**: idempotency under concurrent re-POST is exactly
  the property proposal §2 promises. Without a partial UNIQUE index
  on `(sandbox_id) WHERE state NOT IN ('ok','failed')`, the property
  holds probabilistically (MVCC + operation gap), not deterministically.
- **Action**: (a) add partial UNIQUE index in follow-up migration;
  (b) handle `UNIQUE_VIOLATION` in `wake_sandbox_async_inner` by
  re-running the find lookup + returning the existing wake_id with
  `replay: true`; (c) pg-gated test spawning two
  `wake_sandbox_async_inner` tasks concurrently against the same
  sandbox_id. ~60 LOC. CARRY to R18.

### [R17-T4] Mid-state-machine controller crash leaves wake_jobs row wedged: no `wake_jobs` lessee-takeover sweep; the `lessee` column is INSERTed but never USED

- **Files**: `migrations/0009_wake_jobs.sql:54-55` (`lessee TEXT NOT
  NULL, lessee_updated_at TIMESTAMPTZ`); `admin_handlers.rs:1626-1629`
  (sets `lessee = db.host_id()`); `wake_machine.rs:71-75` (carries
  `lessee`); `sweep.rs:277-340` (`run_wake_jobs_gc_once` deletes only
  TERMINAL rows; no lessee-expiry CAS).
- **Symptom**: controller crashes between
  `set_state(Restoring)` (`wake_machine.rs:269`) and
  `set_state(LivezPolling)` (`:376`). The wake_jobs row sits at
  `state='restoring'` indefinitely. The sandbox-side transient-state-
  takeover sweep (`sweep.rs:151-220`) recovers the SANDBOX row, but
  the WAKE row is never touched. GC only fires on TERMINAL rows
  (`finished_at IS NOT NULL`), so the orphan wedges polling clients
  FOREVER until manual intervention.
- **Cross-reference**: the migration comment at line 56 explicitly
  promises *"PR2 will wire the takeover sweep"* — PR2 ships GC only.
- **Why it matters**: the C-7-LT migration ships a strictly-worse-
  than-sync recovery story for the crash-mid-restore case. Sync path:
  client re-POSTs, sandbox-side sweep eventually recovers. Async path:
  polling client sees `state='restoring'` forever.
- **Action**: (a) `Database::wake_jobs_lessee_expired_inflight(threshold)`;
  (b) sweep CASing lessee + driving WakeMachine recovery (or marking
  row `failed` if sandbox row already moved); (c) pg-gated test
  inserting `restoring` row with stale `lessee_updated_at`, running
  takeover, asserting terminal `failed` + `Internal`. ~120 LOC.
  CARRY to R18.

## MINOR

### [R17-T5] GC race with mid-flight poll: client may receive 404 even though wake completed

- **Files**: `sweep.rs:280-308` (GC); `admin_handlers.rs:1731-1751`
  (poll → 404 on None).
- **Symptom**: T_KEEP=5min; client polls at t=5min+epsilon (gets
  row), GC deletes at t=5min+1, next poll at t=5min+2 gets 404. This
  is documented behaviour per §2.cleanup, but a property test driving
  `(many parallel polls) || (GC at threshold=0)` asserting every
  response is EITHER 200 OK with `agent_url` OR 404 (never a torn
  read, never `state='ok'` with `agent_url=null`) would pin the
  property. ~40 LOC. CARRY to R18.

### [R17-T6] Sweep GC loop has zero unit-test coverage of spawn/shutdown/interval contract; OS-thread detach pattern unit gap (R15-T1b sibling)

- **Files**: `sweep.rs:319-345` (`spawn_wake_jobs_gc` — `detach_isolated`
  loop; observes `state.shutdown_requested()`).
- **Missing**: no test asserts (a) no-op when `state.database.is_none()`;
  (b) shutdown observation between iterations; (c) cadence =
  `WAKE_JOBS_GC_POLL_SECS` (60s). Same defect class as R15-T1b
  (admin_handlers.rs OS-thread detach). The wake_machine integration
  tests call `machine.drive()` DIRECTLY, not via `detach_isolated`, so
  the OS-thread + private-runtime path is still uncovered for the
  wake path too. ~50 LOC. CARRY to R18.

### [R17-T7] PR2 spec-gate coverage matrix vs. api-surface-r16's 5 spec gates: 5-of-5 covered (4 fully, 1 partial)

| Gate | Test | Verdict |
|------|------|---------|
| #1 §10.0 envelope (`error`/`message`) | `r16_api1_failed_state_body_uses_error_and_message_keys` (`admin_handlers.rs:2246-2284`) — body presence + anti-test for deprecated keys | FULL |
| #2 3-char typed_id `wak_` | `wake_prefix_is_three_chars` (`typed_id.rs`) — `WAKE_PREFIX.len()==3` | FULL |
| #3 Idempotency POST status matrix | `wake_machine_terminal_clears_find_pending` (`sandbox_pg_e2e.rs:4296`) — post-terminal branch only; in-flight concurrent branch unpinned per R17-T3 | **PARTIAL** |
| #4 `WakeErrorCode` 1:1 wire codes | `wake_error_code_wire_code_uses_existing_envelope_codes` (`db.rs:3330-3361`) + `r16_api1_failed_state_renders_every_wake_error_code` (`admin_handlers.rs:2289-2310`) — two-layer pin | FULL |
| #5 `?sync=1` deprecation counter | `wake_sync_deprecated_counter_monotonic` (`metrics.rs`) + `wake_query_sync_one_triggers_sync_branch` (`admin_handlers.rs:2380-2393`) | FULL |

Gate #3 promoted to R17-T3 IMPORTANT for the missing concurrent-POST
arm of the matrix.

## Carry-forward (still open from r16)

- **[R10-T2 → R17-T2]** `persist=Some(_)` chain — 9th-round carry,
  promoted above.
- **[R15-T1a]** future-drop coverage of `reserve_vm_index_with_retry`
  — 3rd-round carry. PR2's `detach_isolated` spawn moots ntex-drop for
  async path; sync path remains until Phase 5 deletion.
- **[R15-T1b]** OS-thread detach pattern unit coverage — 7th-round
  byte-coverage-zero at `admin_handlers.rs:1339-1392`. PR2 adds a
  SIBLING `detach_isolated` callsite (`wake-gc` + WakeMachine spawn)
  without unit tests. R17-T6 is the new instance.
- **[R15-T2 → R16-T2]** constant-arithmetic-only test pattern —
  4th-round carry. No new examples this round; pattern surface
  unchanged.
- **[Integration test dir stasis]** **OFF-CARRY THIS ROUND.** Net
  delta `b8fae7b7..c2f24ede` on `crates/sandbox/tests/`: +751 LOC.

Other long-running carries (R9-T6 derive_agent_url parity, R10-T3
read_sandbox_id cross-source, R10-T4 RootKek::from_env, R10-T5
TieredSnapshotStore L2-detach, R9-T3/T5/T8/T9/T10, r4-T2 latency
canary, r3-T3 wrapper-script behavioural, T8 ControllerIdleSnapshotter,
r6 script artifact, r8 sig.rs skew-bypass, R9-P2, R11-T5/R13-T3):
**unchanged this round.** Tracking-only.

## Cross-lens consensus

- **api-surface r16 (R16-API1/2/3 + M2 + counter)** ↔ **test-cov r17
  R17-T7**: ALL FIVE spec gates have ≥1 PR2 test. Pre-implementation
  pre-review worked; PR2 landed clean. Gate #3 has a partial-coverage
  gap (concurrent re-POST) — R17-T3.
- **architecture r17** ↔ **test-cov r17 R17-T4**: wake_jobs lessee-
  takeover gap is a known architectural carry. Migration comment
  promises PR2 wires it; PR2 ships GC only.
- **smoke-r11 cluster lens** ↔ **test-cov r17 (closure)**: smoke-r11's
  "no remaining knob inside synchronous-response contract" verdict is
  receipted by PR2 shipping the structural fix AND the integration
  coverage simultaneously. Pre-Phase-5, sync path still carries
  R15-T1a's future-drop gap.

## Lens hand-off

**To architecture r18**: R17-T4 lessee-takeover + R17-T3 partial UNIQUE
index are architectural calls (sweep cadence, CAS shape, conflict
resolution). test-cov can write fixtures once design is named.

**To concurrency r17 (in-flight)**: WakeMachine `detach_isolated` spawn
at `admin_handlers.rs:1675` is a NEW R15-T1b instance; spawned thread
holds 3 `Arc<dyn …>` + `Option<Arc<Persistence>>`. Verify ownership
transfer clean.

**To test-cov r18**: R17-T2 + R17-T3 + R17-T4 + R17-T6 are the next
sprint's backlog (~310 LOC across 4 pg-gated fixtures + 1 follow-up
migration). R17-T2 closes the oldest open carry (R10-T2, 9 rounds);
R17-T3 closes a NEW idempotency gap; R17-T4 closes the orphan-row gap
the migration's `lessee` column was designed for; R17-T6 closes the
sibling instance of R15-T1b.

## Notes for the next round

**Central question**: did C-7-LT-PR2 close the 8-round EMERGENCY? **YES.**
Both stub failure flags drive `WakeMachine::drive` against a pg-seeded
row, rollback CAS asserted, wire-code mapping pinned, idempotency
invariant checked. R15-T1 sub-asks: 2 CLOSED, 1 carry-forward, 2
partial/mooted.

**R18 default severity**: r16 closed at EMERGENCY; r17 drops to
CRITICAL pending R17-T3 (concurrent re-POST TOCTOU = live integrity
gap) + R17-T4 (orphan row wedges polling clients). Land both with
fixtures in R18 → r19 de-escalates to IMPORTANT.

**Cycle defining pattern (r17 update)**: r13 "firefighting beats
systematic drain." r14 "firefighting now provably misses the fire."
r15 "firefighting has stopped adding tests entirely." r16 "12 tests
on the wrong gaps." **r17 "the structural fix landed, the integration
tests landed WITH it, the wedge surface is finally covered."** The
8-round emergency is over.
