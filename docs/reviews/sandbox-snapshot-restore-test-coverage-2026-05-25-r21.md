# Sandbox/snapshot-restore — test-coverage r21 review

Date: 2026-05-25 (UTC). HEAD at audit: `4d73a5d1`. Round 21. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r20.md`.

## Summary

- 3 NEW findings (1 IMPORTANT + 2 MINOR) + 4 carry-forwards re-filed.
- Sandbox lib tests **426 → 428 (+2)** since r20 audit point
  (`b18782f6`). Intake said `425 → 427`; actual baseline is
  426 → 428 (same +2 delta, off-by-one on both ends). The +2 are
  `wake_job_row_from_pg_returns_error_on_unknown_state_precondition`
  + `wake_job_row_from_pg_error_code_unknown_drops_to_none_not_error`
  in `db.rs:3616-3691`. Verified at HEAD vs r20 commit.
- Integration pg-gated tests **87 → 87 (no change)**. R20-T1
  retry-race test still not added.
- C-7-LT-7 (`03d2f4a8`): the user_id Config-block emission landed
  with the existing `ch_plugin_jobspec_includes_all_task_config_fields`
  test EXTENDED by one assertion line — same over-fulfilment pattern
  flagged in R20-T2. Wire-format pin coverage stays complete.
- **R17-Q3 closure**: the `unwrap_or(Failed)` silent fallback in
  `wake_job_row_from_pg` is gone; `DatabaseError::DataIntegrity`
  variant ships with `.transpose()` at both callers (`db.rs:3164` +
  `db.rs:3263`). LANDED. See R21-T1 for the test-quality caveat.

## Trend table

| Cycle | sandbox lib | pg-gated | Δ lib | Notes |
|-------|-------------|----------|-------|-------|
| r17   | 373         | 74       | +29   | PR1+PR2 |
| r18   | 402         | 83       | +29   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r19   | 424         | 87       | +22   | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| r20   | 426         | 87       | +2    | R19-I4 retry-contract shims + R19-T1 widening |
| **r21** | **428**   | **87**   | **+2**  | r17-Q3 DataIntegrity round-trip + precondition |
| r9→r21 | +132       | +13      | —     | wedge + sweep + sanitizer + probe + retry + drift-guard |

r21 ties r20 for the smallest round in the cycle window. The
remaining surface is **(a) integration depth on freshly-landed paths
(R20-T1)**, **(b) CI plumbing (R18-T8)**, **(c) multi-replica doc
(R19-T4)**, and **(d) the new R21-T1 gap** — none of which are wedge
defects; all are coverage-depth or process gaps.

## CRITICAL

(none — R18-T1 closed r19, R18-T6 downgraded r19, R19-T1 closed r20.)

## IMPORTANT

### [R21-T1] r17-Q3 tests verify the precondition + error variant, not the function itself

- **Files**: `db.rs:3616-3691` (two new unit tests),
  `db.rs:1614-1659` (production `wake_job_row_from_pg` body).
- **Symptom**: the load-bearing property — *"`wake_job_row_from_pg`
  returns `Err(DataIntegrity)` when the row's `state` column holds an
  unknown discriminator"* — is **not directly verified**. The new
  tests inline-acknowledge this (`db.rs:3624-3628`):

  > The `compio_postgres::Row` constructor is `pub(crate)` so we
  > cannot construct a real row in a unit test — instead we pin the
  > precondition (`from_str_opt` returns `None` for unknown strings)
  > and the error variant itself to guarantee the if-None branch is
  > reachable and produces the right type.

  So the tests pin (a) `WakeJobState::from_str_opt("unknown") ==
  None` and (b) `DatabaseError::DataIntegrity(s).to_string()` is
  grep-friendly. They do NOT pin (c) the call site that ties them
  together. A future refactor that drops the `if-None → Err` arm and
  silently substitutes `Failed` would pass both tests.

- **Why now**: same shape as R20-T1 (R19-I4 retry loop pinned by
  enum-shape tests, not loop behaviour). Two consecutive rounds, two
  `pub(crate)` boundaries, two "can't construct input → test
  adjacent invariants" justifications. Pattern is now recurrent.
- **Action**: add `wake_jobs_crud::get_wake_job_returns_data_integrity_
  on_unknown_state_value` to `sandbox_pg_e2e.rs` — INSERT a row with
  a state-bypass (DROP CONSTRAINT → insert → restore, or raw
  `client.execute` outside the typed API), call `db.get_wake_job(id)`,
  assert `Err(DataIntegrity(_))`. ~50 LOC. R22.
- **Severity IMPORTANT**: co-files with R20-T1 under one root cause.
  Both need pg-gated tests in the same sweep, or "unit-test the
  wedge, integration-test separately" hardens into convention.

## MINOR

### [R21-T2] Cluster-side observable invariants are NOT expressed as unit-test constants

- **File**: `crates/sandbox/src/backend/nomad_ch.rs:3287-3289`.
- **Observation** (from r19 retrospective via the prompt): 4 cycles
  of `fence_passed=true` + `probes=2` + `consecutive_misses=2` would
  have caught any drift in the host-fence wedge. The constants ARE
  in source:

  ```
  const MISS_THRESHOLD: u32 = 2;
  const PROBE_CADENCE: Duration = Duration::from_millis(100);
  const CONNECT_TIMEOUT: Duration = Duration::from_millis(150);
  ```

  but they're function-local (inside `wait_for_host_fence`) with no
  unit test pinning them and no `pub(crate) const` export. A
  reviewer reading a cluster log saying "probes=2,
  consecutive_misses=2" must re-derive that 2 is the threshold from
  the function body. A drift to `MISS_THRESHOLD = 3` would silently
  invalidate cluster-log assertions in r19/r20/r21 retros.
- **Why MINOR**: the production constant is single-source-of-truth
  already; this is a reviewer-ergonomics + drift-canary gap, not a
  correctness gap. Cluster logs would still emit whatever the actual
  threshold is — but cross-reference between docs/reviews and source
  becomes an O(log file lookup) rather than O(1 grep).
- **Action**: promote the three constants to `pub(crate) const` at
  module scope and add a `host_fence_observable_invariants` unit
  test in `nomad_ch::tests` asserting `MISS_THRESHOLD == 2`,
  `PROBE_CADENCE == 100ms`, `CONNECT_TIMEOUT == 150ms`. ~15 LOC.
  Pin in the same PR that authors the next cluster-side
  retrospective. R22.

### [R21-T3] Premise-check pattern — third consecutive round with zero adoption

- **Files**: no test file in `crates/sandbox/tests/` carries a
  premise-block header. R20-T3 forwarded this to architecture r21
  for a verdict; architecture's r21 was not part of this audit.
- **Status**: until architecture rules binding-or-not, test-cov
  lens cannot recommend further investment. Carry, do not escalate.
- **Action**: NONE this round. Re-check after architecture r21
  lands a verdict. Auto-delist at r23 if still unresolved.

## Carry-forward from r20

- **R20-T1** R19-I4 retry loop pg-gated coverage — STILL OPEN. No
  new tests in `wake_jobs_crud::*`; the retry property remains
  unit-shape-only. **Recommended shape for r22**: two
  `#[test_with_pg]` tests in `wake_jobs_crud`. (1)
  `insert_wake_job_retries_when_winner_terminal_within_select_window`
  — INSERT pending R0; spawn task with 1ms sleep then mark R0
  terminal; from main task call `insert_wake_job(loser)`; assert
  Ok(Inserted) AND a retry counter incremented. (2)
  `insert_wake_job_returns_validation_after_3_conflicts_with_pending`
  — three concurrent pending writers so loser exhausts the budget;
  assert `Err(Validation{ msg })` with `msg.contains("after 3
  attempts")`. ~80 LOC. Co-file with R19-T3 metric add. CARRY
  IMPORTANT.

- **R18-T5** persist=Some chain — STILL OPEN (no change since r20;
  13th round). CARRY MINOR.
- **R18-T7** sibling probe loops in `probe_and_classify` — STILL
  OPEN. `restore.rs` lib tests unchanged at 2. CARRY MINOR.
- **R18-T8** containerised pg fixture / CI plumbing — **STATUS
  UNCHANGED**. `.github/workflows/ci.yml` carries zero hits on
  `SANDBOX_TEST_PG`, `sandbox_pg`, `wake_jobs_takeover`, or
  `--ignored`. `crates/sandbox/scripts/` still has no `pg-test-up.sh`
  (only 8 unrelated gcp/nomad/lint scripts). The 87 pg-gated tests
  run only on dev boxes. r20's recommendation was "delist after r22
  if no movement"; r21 brings no movement. **FINAL CARRY** before
  delisting at r22 unless infra-lens picks it up. CARRY MINOR.
- **R18-T9** OS-thread detach pattern — 4 sites still uncovered.
  CARRY MINOR.
- **R19-T2** `run_wake_jobs_takeover_once` lib coverage — STILL
  OPEN. No new tests for the no-db noop or Err-fallthrough branch.
  CARRY MINOR.
- **R19-T3** No metric counter for R19-C1 claim — STILL OPEN.
  `metrics.rs` has nothing for wake-job takeover. Co-land with
  R20-T1 race test. CARRY MINOR.
- **R19-T4** Multi-replica precondition — **STATUS UNCHANGED**. No
  `docs/reference/sandbox-wake-jobs.md` file exists (the file does
  not exist at all under `docs/reference/`). The "Why we don't
  unit-test multi-controller takeover" subsection r20 recommended
  has no host file to land in. Lower-effort alternative: append the
  prose to `crates/sandbox/src/sweep.rs` doc-comment at the
  `run_wake_jobs_takeover_once` definition site. ~20 LOC. CARRY
  MINOR; second-final round before delisting.

## Cross-lens consensus

- **R21-T1 ↔ R20-T1** (test-cov internal): both findings share a
  root cause — `pub(crate)` constructor visibility on
  `compio_postgres::Row` and `WakeMachine` internals forces unit
  tests to pin enum/precondition shape rather than function
  behaviour. Cross-finding consensus within test-cov: the next
  PR landing a wedge-adjacent function MUST include a pg-gated
  integration test in the same PR, not a follow-up. Two consecutive
  rounds with this pattern is enough signal.
- **R21-T2 ↔ architecture r19 retrospective**: the cluster-side
  observables (fence_passed/probes/consecutive_misses) are
  architecture's diagnostic vocabulary. Forwarding to architecture
  r22 for a verdict on whether unit tests SHOULD pin them, or
  whether the cluster-log-as-truth pattern is intentional.
- **C-7-LT-7 driver wire-format pin** (driver-side lens
  cross-reference): the user_id Config-block emission landed
  cleanly on the sandbox side with the existing
  `ch_plugin_jobspec_includes_all_task_config_fields` test
  extended by one assertion. The driver-side pin in
  `nomad-driver-ch` (per prompt: "v7 → v8 upload + sandbox-side
  user_id emission test updated") is outside this worktree;
  test-cov lens defers verification to driver-lens.

## Lens hand-off

**To architecture r22**: R21-T2 (cluster-side observable
constants). Question: are MISS_THRESHOLD / PROBE_CADENCE /
CONNECT_TIMEOUT intentionally function-private, or should they
be promoted to module-level `pub(crate) const` with a pinning
unit test?

**To concurrency r22**: R20-T1 carries forward unchanged (no
pg-gated race test landed in r21). Recommended test shape is now
concrete (see Carry-forward block). Concurrency lens to validate
the proposed race-window choreography before authoring.

**To infra/devex (new lens)**: R18-T8 final carry. No CI movement
in r21. Test-cov delists at r22 if no infra-lens pickup.

**To test-cov r22 backlog** (~300 LOC total):
1. R20-T1 pg-gated R19-I4 retry race tests (concrete shape now
   spec'd) — ~80 LOC
2. R21-T1 pg-gated wake_job_row_from_pg DataIntegrity test —
   ~50 LOC
3. R21-T2 host-fence observable-invariant constants + unit test
   — ~15 LOC
4. R19-T4 staging-validation prose appended to `sweep.rs` doc
   comment (no docs/reference file exists) — ~20 LOC
5. R19-T3 takeover claim-count metric + test — ~25 LOC (co-land
   with R20-T1)
6. R18-T5 persist=Some chain (13th carry) — ~80 LOC
7. R18-T7 `probe_and_classify` lib test — ~30 LOC

## Notes for r22

**Central question**: did r20's "co-land wire surface AND pg-gated
integration test in the same PR" lesson generalise? **NO** — r17-Q3
landed in r21 with the same anti-pattern r20 flagged (R20-T1):
outcome-enum + precondition unit tests, no integration test
exercising the function. Two consecutive rounds with the same shape
means the lesson did not stick.

**Recurrent root cause**: `pub(crate)` visibility on input
constructors (`compio_postgres::Row`, `WakeMachine` internal state)
drives unit-test authors to pin adjacent invariants rather than
function behaviour. Either (a) add a `#[cfg(test)] pub` constructor
so unit tests can build inputs, or (b) require pg-gated tests in
the SAME PR for functions with unconstructible input types.
Architecture's call.

**Severity baseline**: r20 had 0 CRITICAL + 1 IMPORTANT + 2 MINOR.
r21 identical. Wedge saturated; remaining work is process + depth.

**Highest-leverage closure for r22**: co-land R20-T1 + R21-T1 +
R19-T3 + R19-T4 + R21-T2 in a single ~190-LOC PR titled "close
r19-r21 coverage backlog". One PR is cheaper than five drips and
forces the integration-test discipline the wedge surface now needs.
