# Sandbox/snapshot-restore — test-coverage r20 review

Date: 2026-05-25 (UTC). HEAD at audit: `b18782f6`. Round 20. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r19.md`.

## Summary

- 4 NEW findings (1 POSITIVE-closure + 1 IMPORTANT + 2 MINOR)
  + 4 carry-forwards re-filed.
- Sandbox lib tests **424 → 426 (+2)** since r19 audit point
  (`bffa6f1d`). The intake assumed 425; the r19 trend table itself
  shows 424. The +2 comes from R19-I4's two retry-contract unit
  tests in `db.rs` (`insert_wake_job_retries_on_winner_going_
  terminal_unit`, `insert_wake_job_returns_error_after_max_retries_
  message_shape`), landed in commit `f2485210` which is AFTER the
  r19 audit cutoff. R19-T1 (`6fbfafb3`) widened an existing 7-entry
  loop to 8 entries — zero new test functions — exactly as the
  intake described.
- Integration pg-gated tests **87 → 87 (no change)**.
- **R19-T1 CLOSED**: `admin_handlers.rs:2324-2332` 8-entry array now
  includes `WakeErrorCode::WakeWorkerAborted`; the commit also added
  per-iteration assertions on `state == "failed"` and
  `message.is_string()` — a bonus envelope-shape tightening that
  wasn't part of the r19 recommendation. Verified inline.
- **R19-I4 retry contract** lib-tested but NOT pg-tested. The two
  new lib tests assert outcome-enum + error-message shape; the real
  retry loop (3 attempts, `DatabaseError::Validation` on exhaustion)
  has no integration coverage. NEW IMPORTANT — see R20-T1.

## Trend table

| Cycle | sandbox lib | pg-gated | Δ lib | Notes |
|-------|-------------|----------|-------|-------|
| r17   | 373         | 74       | +29   | PR1+PR2 |
| r18   | 402         | 83       | +29   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r19   | 424         | 87       | +22   | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| **r20** | **426**   | **87**   | **+2**  | R19-I4 retry-contract shims + R19-T1 widening |
| r9→r20 | +130       | +13      | —     | wedge + sweep + sanitizer + probe + retry |

r20 is the smallest round in the entire 11-cycle window. The wedge
surface is now saturated; remaining gaps are CI plumbing (R18-T8),
multi-replica validation (R19-T4), and integration-test depth on
fresh-landed paths (R20-T1).

## CRITICAL

(none — R18-T1 closed r19, R18-T6 downgraded r19, R19-T1 closed r20.)

## IMPORTANT

### [R20-T1] R19-I4 retry loop has zero pg-gated coverage; lib tests are outcome-shape stubs

- **Files**: `db.rs:3039-3122` (production retry loop, 3 attempts);
  `db.rs:4087-4166` (two new unit tests). The unit tests construct
  `InsertWakeJobOutcome::Inserted` / `Replay(winner)` /
  `DatabaseError::Validation` directly and assert on enum shape and
  error-message substrings. They never invoke the retry loop.
- **Symptom**: the load-bearing property — *"on conflict + None from
  `find_pending_wake_for_sandbox`, the loop re-issues the INSERT
  and lands on the second/third attempt"* — is unverified. A future
  refactor that drops the `for attempt in 0..MAX_RETRIES` loop and
  returns `Err` immediately would pass all current tests. The error-
  message-substring test only fires when retry-exhaustion *also*
  produces a `DatabaseError::Validation`; the loop's existence is
  not pinned.
- **Why now**: the prompt says R19-I4 was "verified in r19" but
  inspection of the audit-point commit `bffa6f1d` shows the R19-I4
  commit `f2485210` landed AFTER it. r19 could not have verified
  these tests. The pg-gated suite `wake_jobs_crud::*` has 4 R19-C1
  tests but zero R19-I4 tests. The retry path needs a pg-gated test
  that races a winner-to-terminal transition against the INSERT
  caller's SELECT.
- **Action**: add `wake_jobs_crud::insert_wake_job_retries_when_
  conflict_winner_goes_terminal_within_select_window` — set up a
  pending row, transition it to terminal via the WakeMachine in a
  spawned task, race `insert_wake_job(loser)` against it, assert
  the loser returns `Inserted` (not `Replay`, not `Err`). ~80 LOC.
  Plus a "third attempt also conflicts" variant pinning the
  `Err(Validation)` exhaustion path. R21.
- **Severity IMPORTANT** because the retry loop is the sole defense
  against a sub-ms TOCTOU window that previously returned 500 to
  every wake-on-cold-sandbox caller hitting the race. The unit tests
  give *false* confidence: they pass even if the loop is removed.

## MINOR

### [R20-T2] R19-T1 closure also tightened envelope-shape per iteration — undocumented test improvement

- **File**: `admin_handlers.rs:2343-2351`.
- **Observation**: the r19 recommendation was "~1 LOC add". The
  actual commit added 9 LOC: the new variant entry plus two extra
  asserts per loop iteration (`state == "failed"`,
  `message.is_string()`). This is a bonus envelope tightening that
  closes a latent gap r19 didn't call out — the prior test only
  pinned `body["error"]`, leaving `state` and `message` drift-only.
- **Why MINOR**: positive surprise; not a gap. Worth logging because
  it suggests the implementer routinely over-fulfils the rec — a
  pattern reviewers should EXPECT going forward when proposing
  one-LOC closures.
- **Action**: none. Note in retrospective.

### [R20-T3] Premise-check invariant template — still NOT adopted in unit tests

- **Files**: no test file in `crates/sandbox/tests/` carries a
  premise-block header. The pattern is used in cluster review docs
  (`docs/reviews/*-architecture-*`) but not in `sandbox_pg_e2e.rs`,
  `wake_machine_e2e.rs`, or the lib `#[cfg(test)] mod tests`
  blocks.
- **Why now**: this is the second consecutive round (r19 + r20)
  raising it. The recommendation has had 0 LOC of follow-through.
  Either the team has chosen not to adopt the pattern (in which
  case retire the recommendation) or it's deferred indefinitely (in
  which case escalate to architecture lens for a verdict).
- **Action**: cross-lens query to architecture r20 — "is the
  premise-check pattern an aspiration or a binding template?" If
  binding: add a header block to `sandbox_pg_e2e.rs` listing wake-
  machine-e2e's three load-bearing premises (lessee_updated_at
  fresh-on-insert; unique-partial-index excludes terminal;
  WakeMachine state transitions are observable from outside the
  spawned task). ~20 LOC. R21.

## Carry-forward from r19

- **R18-T5** persist=Some chain — STILL OPEN (no change since r19;
  12th round). CARRY MINOR.
- **R18-T7** sibling probe loops (`probe_and_classify` in
  `restore.rs:570`) — STILL OPEN. `restore.rs` lib tests unchanged
  at 2. CARRY MINOR.
- **R18-T8** containerised pg fixture / CI plumbing — STILL OPEN.
  `crates/sandbox/scripts/` lists 8 scripts (`bake-rootfs.sh`,
  `gcp-*.sh`, `init.sh`, `lint.sh`, `nomad-vm-wrapper.sh`,
  `provision-*.sh`, `teardown-*.sh`) — no `pg-test-up.sh`.
  `.github/` carries zero hits on `SANDBOX_TEST_PG`, `sandbox_pg`,
  or `--ignored`. The 87 pg-gated tests run ONLY on dev boxes.
  **ETA recommendation: out-of-band side quest, ~150 LOC + 1 CI
  job, NOT blocking on r21 — file a tracking issue and assign to
  infra-lens, then drop from test-cov backlog after r22.** CARRY
  MINOR (final round before delisting).
- **R18-T9** OS-thread detach pattern — 4 sites still uncovered.
  CARRY MINOR.
- **R19-T2** `run_wake_jobs_takeover_once` lib coverage — STILL
  OPEN. No new tests for the no-db noop or Err-fallthrough branch.
  CARRY MINOR.
- **R19-T3** No metric counter for R19-C1 claim — STILL OPEN.
  `metrics.rs:161` still has nothing for wake-job takeover (only
  the older transient-takeover counter). CARRY MINOR.
- **R19-T4** Multi-replica precondition — DOCUMENTED ACCEPTABLE
  GAP. **Staging-validation plan**: empirically pin via the smoke
  cluster's two-controller deploy (T-8b cluster runs 2x controllers
  with shared pg). The acceptance criterion is a 10-minute window
  where both controllers process the same `wake_jobs_takeover` tick
  with `claim_orphan_wake_for_recovery` and PG row-locks serialize
  them at the SQL layer (observable as exactly-one claim metric
  increment per orphan row). Document in
  `docs/reference/sandbox-wake-jobs.md` under a new "Why we don't
  unit-test multi-controller takeover" subsection. ~30 LOC of
  prose; smoke validation already implicit in T-8b runs. RETIRE
  after r22.

## Cross-lens consensus

- **R19-T1 closure** ↔ **api-surface-r19 R19-API1**: both observed
  the same commit pair (`6fbfafb3` + `fde4f51c`). The rephrased
  `error_message` text and the wire-code coverage now align — the
  taxonomy is locked end-to-end (SQL literal → `db.rs` round-trip
  test → `admin_handlers.rs` renderer test). Unanimous CLOSURE.
- **R20-T1** ↔ **concurrency-r20**: the R19-I4 retry loop is a
  concurrency-adjacent invariant (sub-ms TOCTOU). Cross-lens
  consensus needed on whether the unit-test pair is sufficient or
  whether a pg-gated race test is required. test-cov vote: NOT
  sufficient (the loop's existence isn't pinned). Forwarding to
  concurrency r21 for second opinion.
- **C-7-LT-4 driver-side coverage** (sandbox-side lens cross-
  reference): the prompt cites "95 driver tests including 6 new
  `RewriteRestoreConfigPaths` + integration". Inspection of
  `nomad-driver-ch/tests/*_test.go` (3 files, 13 `^func Test*`
  matches) does NOT corroborate that count, and no file in the
  driver tree matches `RewriteRestoreConfigPaths` (grep returns 0
  hits). The cited 95 figure may include subtests / table-driven
  sub-cases not visible to `^func Test*` patterns, OR the driver
  pin v6 (`dea68995`) refers to remote test sources not vendored
  here. Flag for cross-lens disambiguation: the driver-side count
  in the intake is unverifiable from this worktree. Sandbox-side
  test-cov lens defers driver coverage to driver-lens.

## Lens hand-off

**To architecture r21**: R20-T3 (premise-check pattern adoption)
needs an architecture verdict. Two rounds of "recommended, not
adopted" should resolve.

**To concurrency r21**: R20-T1 (R19-I4 retry loop pg-gated
coverage gap) — concurrency lens to sign off on whether the lib-
only outcome-enum tests are sufficient or a real race test is
required. Test-cov lens votes "real race test required".

**To code-quality r21**: R20-T2 (R19-T1 over-fulfilment) is a
positive pattern note. No follow-up required.

**To api-surface r21**: R19-API1 rephrased the `error_message`
content. Lens should pin the new text in a `wire_format_constants`
or `error_message_table` test if such a thing exists. Otherwise:
file as a doc-as-contract concern.

**To infra/devex (new lens)**: R18-T8 CI plumbing for pg-gated
tests. Out-of-band side quest. Test-cov lens recommends delisting
after r22 if no movement.

**To test-cov r21 backlog** (~285 LOC total):
1. R20-T1 pg-gated R19-I4 retry race test — ~80 LOC
2. R20-T3 premise-check header block (if architecture endorses) —
   ~20 LOC
3. R18-T5 persist=Some chain (12th carry) — ~80 LOC
4. R18-T7 `probe_and_classify` lib test — ~30 LOC
5. R18-T8 containerised pg (delist candidate) — ~150 LOC
6. R18-T9 detach helper + 4-site tests — ~50 LOC
7. R19-T2 `run_wake_jobs_takeover_once` lib unit tests — ~30 LOC
8. R19-T3 takeover claim-count metric + test — ~25 LOC
9. R19-T4 staging-validation doc subsection — ~30 LOC

## Notes for r21

**Central question**: did r19's "wire-format response renderer must
ship WITH the SQL surface" lesson generalise? **YES on the wire
side** (R19-T1 closed in one round) **but NO on the integration
side** (R19-I4 landed unit-only, with no pg-gated equivalent of the
SQL-race property). The next sweep / handler landing should pin
BOTH the wire surface AND a pg-gated integration test in the same
PR.

**r20 severity baseline**: r19 had 0 CRITICAL + 1 IMPORTANT + 3
MINOR. r20 has 0 CRITICAL + 1 IMPORTANT + 2 MINOR. The calmness
trend that started at r17 continues — r20 is the calmest round
since r9.

**Cycle-defining pattern**: r20 = "the wire surface closed cleanly
in one round; the integration surface for a freshly-landed retry
loop is unit-only." For sub-ms TOCTOU windows, unit-only tests
that construct enum values directly do NOT prove the loop runs.
Future race-loop landings: include a pg-gated race test in the
SAME PR.

**Highest-leverage gap for r21**: R20-T1 (pg-gated R19-I4 retry
test). 80 LOC, closes integration coverage on a load-bearing
TOCTOU defense. Co-land with R19-T3 metric if concurrency-r21
endorses.
