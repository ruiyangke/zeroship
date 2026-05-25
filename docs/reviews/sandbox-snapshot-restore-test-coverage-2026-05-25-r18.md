# Sandbox/snapshot-restore — test-coverage r18 review

Date: 2026-05-25 (UTC). HEAD at audit: `87f40229` (smoke-r13 retro
filed; C-7-LT-2 NEW). Round 18. Prior: `docs/reviews/sandbox-snapshot-
restore-test-coverage-2026-05-25-r17.md`.

## Summary

- 9 NEW findings (2 POSITIVE, 1 CRITICAL, 4 IMPORTANT, 2 MINOR)
  + 4 carry-forwards re-filed.
- Sandbox lib tests **373 → 402 (+29)** since r17 across PR2-FOLLOWUP +
  GATE-C2 + C-7-LT-1 + visibility tightening. Most of the delta is
  concentrated on the wedge surface (`wake_machine.rs` 3 → 22 tests).
- Integration pg-gated tests **74 → 83 (+9)** in `sandbox_pg_e2e.rs`:
  4 new GATE-C2 `wake_jobs_crud` tests (`wake_jobs_insert_returns_
  inserted_on_fresh_sandbox`, `…_collapses_concurrent_race_via_unique_
  index`, `…_unique_index_releases_after_terminal_transition`, `…_
  sandbox_pending_uniq_index_present`) + 5 pre-existing.
- 3 C-7-LT-1 lib tests landed at `restore_handler.rs:1764/1795/1827`:
  async@30 = 70s budget / 36 attempts; sync@30 = C-8b 50s / 26
  attempts pin; async@120 = 250s / 126 attempts (deadline unbind).
- **R17-T3 → CLOSED by GATE-C2.** The TOCTOU race is now provably
  collapsed by migration 0011's partial UNIQUE INDEX + ON CONFLICT
  DO NOTHING. Three integration tests pin the property; the loser's
  wake_id MUST NOT land in pg.
- **R17-T2 → STILL OPEN.** All 6 wake_machine_e2e tests still pass
  `persist: None` (`sandbox_pg_e2e.rs:4709`); 3 wire codes
  (`ClockResyncFailed`, `RegisterFailed`, …) remain drift-only.
- **NEW R18-T1 CRITICAL**: zero unit tests pin the probe-loop
  wall-time cadence at audit HEAD. Smoke-r13 (`probes=1, elapsed_ms=
  30129`) would have been caught locally by a 30-line compio test
  against an unroutable IP. The probe-wedge fix landed POST-HEAD at
  `40811d8b`, carrying 5 new tests; this round files the LESSON, not
  the gap (the gap is fixed in the very next commit beyond audit HEAD).

## Trend table

| Cycle | sandbox lib | pg-gated | Δ lib | Notes |
|-------|-------------|----------|-------|-------|
| r9    | 296         | (no wake) | — | baseline |
| r16   | 344         | (no wake) | +12 | C-8/A1 |
| r17   | 373         | 74        | +29 | PR1+PR2 |
| **r18** | **402**   | **83**    | **+29** | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r9→r18 | +106       | +9 (this cycle) | — | wedge surface now load-bearing tested |

+29 ties r17's record. Two consecutive rounds of audit-record-large
lib deltas, both concentrated on wake/restore code. The 8-round
EMERGENCY (r10-r16) is structurally over.

## CRITICAL

### [R18-T1] No unit/lib test pins probe-loop wall-time vs. unreachable target at audit HEAD; smoke-r13 wedge was production-discoverable only

- **Files at HEAD**: `crates/sandbox/src/backend/nomad_ch.rs:3184`
  (`wait_for_agent_silent`); 62 tests in the file at audit HEAD,
  zero assert probe-loop wall-time against a black-hole address.
- **What smoke-r13 caught**: 1 probe in 30 s instead of the designed
  ~300 at 100 ms cadence — `ureq::Agent::timeout()` is a request-
  deadline, not a connect-deadline, and TCP-connect to a half-
  collapsed TAP route hangs at the kernel's SYN-retransmit ceiling
  (~30 s on Linux).
- **What would have caught it locally**: a `#[compio::test]` firing
  the inner probe against RFC 5737 TEST-NET-1 (`192.0.2.1:7777`) and
  asserting wall-time `<1 s`. That test exists POST-HEAD at
  `nomad_ch.rs:5202` (`pr1_probe_unroutable_address_returns_false_
  within_timeout`, landed in `40811d8b` — 1 commit beyond audit
  HEAD), with `assert!(elapsed < Duration::from_millis(750))`.
- **Why CRITICAL at this HEAD**: 30 s × 1 slot LEAK per teardown
  in production; an iso-cost lib test would have caught it on the
  first `cargo test` after `wait_for_agent_silent` landed. Smoke-
  r13 was the only signal source; the audit-HEAD has 0 such tests.
- **Lesson**: every blocking-IO probe loop with a "designed N
  probes / interval / budget" docstring MUST land WITH a wall-time
  test against a stuck target. Introduce a helper `expect_probe_
  loop_completes_within(target, budget, max_wall_ms)` for all 3
  probe loops (`wait_for_agent_silent`, `wait_for_agent_livez`,
  `probe_and_classify`).
- **Action**: STATUS-CLOSED-AT-`40811d8b` for `wait_for_agent_
  silent`; CARRY as r18-pattern-lesson for siblings (~80 LOC).

## IMPORTANT

### [R18-T2] GATE-C2 integration coverage closes R17-T3 — POSITIVE

The 4 new tests in `sandbox_pg_e2e.rs:4372-4577` are the audit's
preferred shape:

- `wake_jobs_insert_returns_inserted_on_fresh_sandbox` (`:4374`):
  `InsertWakeJobOutcome::Inserted` happy path.
- `wake_jobs_insert_collapses_concurrent_race_via_unique_index`
  (`:4413`): two-INSERT race; loser's wake_id MUST NOT land in pg
  (`:4448`: `get_wake_job("wak_c2_loser").is_none()`).
- `wake_jobs_unique_index_releases_after_terminal_transition`
  (`:4465`): fresh INSERT succeeds after winner reaches terminal.
- `wake_jobs_sandbox_pending_uniq_index_present` (`:4538`):
  schema pin via `pg_indexes` introspection.

The race test simulates concurrency via back-to-back INSERTs on
the same handle (comment at `:4406` is explicit about the
scaffolding trade-off); post-state invariant (one row, agreed
wake_id) is load-bearing. Schema-pin catches the case where
migration 0011 silently fails (no arbiter → ON CONFLICT no-op).

R17-T3 materially CLOSED at `db248cbf`.

### [R18-T3] C-7-LT-1 lib tests well-shaped; empirical envelope asserted via a single smoke-r12 wall-time

`restore_handler.rs:1764-1857` — all 3 sub-asks pinned: async@30 =
36 attempts / 70s; sync@30 preserves C-8b at 26 attempts / ≤50s;
async@120 = 126 attempts (with cross-check `p_async > p_sync`).

Hardening for r19 (~13 LOC total):
- `wall_ms >= 60_166` hardcodes a single smoke measurement. If
  upstream teardown shifts, test passes silently. Replace with
  "budget MUST exceed observed_max + 10s headroom" constant.
- `c7_lt_1_async_mode_fence_30_yields_70s_budget` is missing the
  mode-split `assert!(async > sync)` guard that fence=120 has.

Strong positive overall.

### [R18-T4] `make_machine` test fixture discards `InsertWakeJobOutcome` (cross-lens with code-quality-r18-I1)

- **File**: `sandbox_pg_e2e.rs:4704` (the `db.insert_wake_job(&row)
  .await.unwrap()` call inside `make_machine` defined at L4677).
  code-quality-r18-I1 cites `:4674`; actual line is 4704.
  Unanimous cross-lens consensus on the fix.
- **Symptom**: post-GATE-C2 signature is `Result<InsertWakeJobOutcome>`.
  If test-ordering leaves a non-terminal row, the call returns
  `Replay(old_row)` and `make_machine` drives the OLD wake_id
  while asserting on the NEW one — surfacing as a confusing
  `wake_job row must exist after drive` panic.
- **Same pattern**: 11 other discard-sites in pg_e2e; most are
  in `wake_jobs_crud` tests that immediately assert state (safe).
  `make_machine` is the only fixture that THEN drives a state
  machine — the only one that NEEDS the explicit `Inserted` check.
- **Action**: `assert!(matches!(…, InsertWakeJobOutcome::Inserted),
  "make_machine must start from fresh INSERT — prior test leaked
  a pending row")`. ~1 LOC. Land in R19.

### [R18-T5] R17-T2 carry — `persist=Some(_)` chain still uncovered (10th-round carry from R10-T2)

- **Files**: `wake_machine.rs:392-422` (unseal/clock_resync/register
  branch); `sandbox_pg_e2e.rs:4709` (every wake_machine_e2e test
  hardcodes `persist: None`).
- **Symptom unchanged**: `WakeErrorCode::{ClockResyncFailed,
  RegisterFailed}` drift-pinned but never DRIVEN. 4-of-7 wire
  codes integration-tested; 3 remain drift-only after 10 rounds.
- **PR2-FOLLOWUP didn't address this**: 28 new lib tests sit on
  state-machine internals + classify + config + sanitizer — none
  wires a real `Persistence`.
- **Action**: 7th fixture stub-failing `Persistence::unseal` or
  `::register_restored`. ~80 LOC. CARRY R19.

### [R18-T6] R17-T4 carry — wake_jobs `lessee`-takeover sweep still has zero tests + zero implementation; severity ESCALATED post-GATE-C2

- **Files**: `sweep.rs:280-345` GC only; migrations/0009:56 still
  promises "PR2 will wire the takeover sweep". PR2-FOLLOWUP shipped
  lessee bump (R17-A1) but NOT the takeover.
- **GATE-C2 amplifies this**: with the partial UNIQUE INDEX now
  blocking duplicate INSERTs while a wedged-restoring row sits in
  pg, the orphan permanently BLOCKS new wakes for the same sandbox
  until GC sees a terminal state — which never comes. Pre-GATE-C2
  the client could retry-into-a-fresh-wake_id (CAS-loser failure
  mode); post-GATE-C2 the retry path is `ON CONFLICT → Replay
  (stale_row)` forever.
- **Action**: `Database::wake_jobs_lessee_expired_inflight(threshold)`
  + sweep CAS-driving recovery. ~120 LOC + ~50 LOC test. CARRY R19.
  **Severity ESCALATED IMPORTANT → LATENT-CRITICAL.**

## MINOR

### [R18-T7] Sibling probe loops still uncovered (R18-T1 generalisation)

Three probe-loop sites: (1) `nomad_ch.rs:3184` `wait_for_agent_
silent` — TESTED POST-HEAD at `40811d8b`; (2) `nomad_ch.rs`
`wait_for_agent_livez` — UNTESTED; (3) `restore.rs:309`
`probe_and_classify` — UNTESTED. Apply R18-T1 lesson to siblings
2+3 (TEST-NET-1 + wall-time bound). ~30 LOC each. CARRY R19.

### [R18-T8] Local pg fixture not containerised; pre-existing wake_machine_e2e failures are env-only

`cargo test --test sandbox_pg_e2e -- --ignored` requires
`SANDBOX_TEST_PG=postgres://…:5440/…`; produces "failures" when
port unreachable. Connection-refused, not logic — but pollutes
runs and trains reviewers to ignore. **Structural fix**: ship
`crates/sandbox/scripts/pg-test-up.sh` (compose-up pinned PG +
fixed port + schema reset), couple to `cargo test` via a
ctor-style port-probe. CI integration tests need a pg sidecar;
without it, the +9 GATE-C2 tests never run in CI. ~150 LOC.
CARRY R19.

### [R18-T9] R15-T1b OS-thread detach pattern — 8th-round zero-test carry

Three `detach_isolated` callsites: `admin_handlers.rs:1339-1392`
(R15-T1b), `:1675` (PR2 WakeMachine spawn), `sweep.rs:319-345`
(PR2 GC). Every wake_machine_e2e test bypasses via `machine.drive
().await`. PR2 added 2 sites without coverage. Extract
`detach_isolated_for_test(state, body)` + 1 test per invariant.
~50 LOC. CARRY R19.

## Carry-forward (unchanged from r17)

- **R17-T2** → R18-T5 (10th-round carry).
- **R17-T4** → R18-T6 (severity ESCALATED).
- **R17-T5 GC race with mid-flight poll** → unchanged, MINOR carry.
- **R17-T6 OS-thread detach pattern** → R18-T9 (8th round).
- **R15-T1a future-drop sync path** → unchanged until Phase 5
  deletion.
- **R15-T2 / R16-T2 constant-arithmetic-only test pattern** →
  unchanged, no new examples this round.

Long-running carries (R9-T6, R10-T3/T4/T5, R9-T3/T5/T8/T9/T10,
r4-T2, r3-T3, T8, r6, r8, R9-P2, R11-T5/R13-T3): unchanged this
round, tracking-only.

## Cross-lens consensus

- **code-quality-r18 R18-I1** ↔ **test-cov r18 R18-T4**:
  unanimous — `make_machine` discarding `InsertWakeJobOutcome` is
  the same line, same fix, same severity. Land 1-LOC assert in
  R19.
- **concurrency-r18 R18-C1 (R17-C2 carry)** ↔ **test-cov r18
  R18-T2**: concurrency-r18 flagged R17-C2 as "OPEN, GATE-C2
  queued for r20"; GATE-C2 landed at `db248cbf` BEFORE r20 and
  test-cov r18 closes the gap with 3 new pg-gated tests. CLOSED.
- **architecture-r18 R18-A1 (wedge shape)** ↔ **test-cov r18
  R18-T1**: architecture-r18 named the wedge; test-cov r18 names
  the missing test. The probe wedge would have been a 30-line
  unit test; the architectural-fix-without-test pattern is the
  cycle-defining issue for r18.
- **smoke-r13 cluster lens** ↔ **test-cov r18 R18-T1**: smoke-r13
  IS the production signal that caught what 0 lib tests caught.
  R18-T1 is the lesson: every probe-loop docstring with "designed
  N probes / interval / budget" MUST land with a wall-time test
  against a deliberately stuck target.

## Lens hand-off

**To architecture r19**: R18-T6 wake_jobs lessee-takeover is now
LATENT-CRITICAL post-GATE-C2 (the UNIQUE INDEX amplifies the
orphan-wedges-sandbox failure mode). architecture call needed:
sweep cadence + CAS shape + recovery vs. mark-failed.

**To code-quality r19**: R18-T4 (`make_machine` outcome discard) is
trivially-actionable; co-land with the R19 wake_machine_e2e new
test for R18-T5 (persist chain).

**To concurrency r19**: R18-T9 OS-thread detach pattern has 3
sites now (was 1 at r15). Cluster the test-helper extraction.

**To security r19**: R18-T1's wedge-as-availability-bug shape (slot
LEAK on every teardown → DoS-via-natural-traffic) is security-
adjacent. Probe-loop wall-time bounds are a security invariant,
not just a correctness one.

**To test-cov r19**: backlog (~470 LOC across 6 fixtures):
1. R18-T5 persist chain — ~80 LOC
2. R18-T6 lessee-takeover sweep + test — ~170 LOC
3. R18-T7 sibling probe loops — ~60 LOC × 2 = 120 LOC
4. R18-T8 containerised pg fixture — ~150 LOC (out-of-band)
5. R18-T9 OS-thread detach helper + tests — ~50 LOC
6. R18-T4 + R18-T3 nits — ~15 LOC

## Notes for r19

**Central question**: did r17 EMERGENCY-de-escalation hold? **YES.**
R17-T3 CLOSED by GATE-C2 (+3 integration tests); C-7-LT-1 lib
coverage landed cleanly (3 tests); PR2-FOLLOWUP added 28 lib
tests on the wedge surface.

**r19 severity baseline**: r17 IMPORTANT → r18 CRITICAL pending
R18-T1 sibling generalisation (already half-addressed at
`40811d8b`). Re-baseline to IMPORTANT once R18-T7 lands.

**Cycle-defining pattern**: r17 "structural fix landed WITH
integration tests." **r18 "the API wedge surface is covered but
the upstream PROBE LOOP wasn't — smoke-r13 caught what 0 lib
tests caught."** New rule: every blocking-IO loop with designed
N-probes / interval / budget ships WITH a stuck-target wall-time
test, full stop.

**Highest-leverage gap for r19**: R18-T6 (lessee-takeover) —
GATE-C2's UNIQUE INDEX converts wedged-orphan rows from "client
re-POSTs with fresh wake_id" to "client permanently blocked
until manual intervention." Architecture-r19 needs to name the
sweep shape; test-cov can fixture once design lands.
