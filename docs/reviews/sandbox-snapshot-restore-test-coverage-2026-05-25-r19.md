# Sandbox/snapshot-restore — test-coverage r19 review

Date: 2026-05-25 (UTC). HEAD at audit: `bffa6f1d` (R19-C1 takeover
sweep landed; R19-I1 two-phase probe landed). Round 19. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r18.md`.

## Summary

- 7 NEW findings (3 POSITIVE-closures + 1 IMPORTANT + 3 MINOR)
  + 4 carry-forwards re-filed.
- Sandbox lib tests **402 → 424 (+22)** since r18 (matches the
  +27-ish window across the post-PR2-FOLLOWUP period claimed in the
  intake; the +5 delta vs. intake's 423 is one extra fence-cadence
  test landed alongside C-7-LT-2). Concentrated on probe loops
  (`nomad_ch.rs` +9), sanitizer (R17-S1 +6), retry budget (C-7-LT-1
  +3), and R19-C1 lib pins (+6 across `config`/`sweep`/`metrics`).
- Integration pg-gated tests **83 → 87 (+4)**: all 4 are the
  R19-C1 `wake_jobs_crud::claim_orphan_*` group in `sandbox_pg_e2e.rs:
  4677-4904`.
- **R18-T1 CLOSED** by `pr1_probe_unroutable_address_returns_false_
  within_timeout` (`nomad_ch.rs:5202` area, landed in `40811d8b`).
  Verified inline (`cargo test … --list` shows the test).
- **R18-T6 (lessee-takeover) DOWNGRADED LATENT-CRITICAL → MINOR**:
  4 pg-gated tests pin the SQL claim semantics; 2 lib tests pin
  cadence + threshold floor; sweep `run_wake_jobs_takeover_once`
  itself remains lib-untested (gap, but not load-bearing).
- **R18-T5 still OPEN**: only 1 hit on `persist: Some(` in pg_e2e
  vs. 1+ on `persist: None`; 3 wire codes still drift-only after
  11 rounds.
- **NEW R19-T1 IMPORTANT**: `WakeWorkerAborted` is half-wired in the
  taxonomy tests — `wake_error_code_as_str_round_trip` and
  `wake_error_code_wire_code_uses_existing_envelope_codes` (db.rs)
  include it, but the admin-handler integration test
  `r16_api1_failed_state_renders_every_wake_error_code` does NOT.
  The takeover-claimed row's poll response is not pinned to a wire
  code.

## Trend table

| Cycle | sandbox lib | pg-gated | Δ lib | Notes |
|-------|-------------|----------|-------|-------|
| r17   | 373         | 74       | +29   | PR1+PR2 |
| r18   | 402         | 83       | +29   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| **r19** | **424**   | **87**   | **+22** | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| r9→r19 | +128       | +13      | —     | wedge + sweep + sanitizer + probe |

Three consecutive +20-plus rounds; r19 is the smallest of the three
because the wedge surface is now saturated.

## CRITICAL

(none — R18-T1 closed; R18-T6 downgraded post-R19-C1.)

## IMPORTANT

### [R19-T1] Admin-handler poll renderer not pinned for `WakeWorkerAborted` wire code

- **Files**: `admin_handlers.rs:2319-2343`
  (`r16_api1_failed_state_renders_every_wake_error_code`), 7-variant
  loop omitting `WakeWorkerAborted`; `db.rs:3505-3523` round-trip,
  3525-3564 wire-code table — both updated for R19-C1.
- **Symptom**: the taxonomy unit tests cover the new variant, but
  the integration-shaped admin renderer test does NOT. If a future
  refactor of `render_wake_poll_response` accidentally drops or
  re-spells the `wake_worker_aborted` wire code in the response
  body, only the SQL round-trip test fires; the wire surface that
  the AI builder will inspect goes silently wrong.
- **Why now**: R19-C1's whole point is to convert wedged-orphan rows
  into a clean `failed` + `wake_worker_aborted` poll response so
  clients can retry on a fresh wake_id. The poll-renderer is the
  load-bearing surface, and it's the one that's NOT under test for
  the new code.
- **Action**: add `WakeErrorCode::WakeWorkerAborted` to the 7-entry
  array at `admin_handlers.rs:2324-2331`. ~1 LOC. R20.
- **Severity IMPORTANT** because the path is hot (the takeover sweep
  fires every 60 s in production once enabled) and the missing
  assertion is at the wire-format surface.

## MINOR

### [R19-T2] `run_wake_jobs_takeover_once` itself has no lib test

- **File**: `sweep.rs:388-423`. The function reads
  `state.database` + threshold and calls into the SQL claim. SQL
  semantics are pg-gated (4 tests); the dispatcher branch (`db is
  None` → 0; `Err` → 0 with warn log) is unreachable from the
  pg-gated suite by design (those tests use a real db).
- **Symptom**: the no-db noop and the Err-fallthrough are *only*
  exercised in production. A regression that returns `n` even when
  `Err(_)` is folded into the success branch by mistake would skew
  metrics (once a metric counter is added — see R19-T3 below).
- **Action**: 2 small unit tests via a trait-shaped fake or by
  passing a `None` `AppState` shim. ~30 LOC. R20.

### [R19-T3] No metric counter for R19-C1 claim — only `tracing::warn!`

- **Files**: `sweep.rs:394-413` (claim → `tracing::warn` only);
  `metrics.rs:161` (`add_takeover_lease_expiration` for transient
  takeover; nothing for wake-job takeover).
- **Symptom**: nothing for the operator to alert on. The smoke-r13
  retrospective explicitly called out **phase-level histograms** as
  a recommendation (`docs/reviews/…-architecture-2026-05-25-r19.md:
  138` — "alert on `_p99 > 25`; (3) document the premise-check
  pattern"). The R19-C1 takeover is exactly the place where
  cardinality of claim events per minute should be observable.
- **Action**: add `metrics::add_wake_job_takeover_claimed(n)` + an
  in-sweep call + a sibling `add_takeover_zero_is_noop`-style test.
  Cross-cut with concurrency-r19's metric recommendation. ~25 LOC.
  R20.

### [R19-T4] Multi-replica precondition for R19-C1 has zero test coverage; documented gap in fixture comment

- **File**: `sandbox_pg_e2e.rs:4844-4904`
  (`claim_orphan_wake_concurrent_claims_race_cleanly`). The test
  comment at L4872-4878 is explicit: *"Under compio's single-threaded
  runtime they execute sequentially at the SQL boundary; the
  property the test pins is correctness, not parallelism."*
- **Symptom**: postgres row-locks DURING the takeover UPDATE
  (`db.rs:3289-3308`) are claimed to serialize concurrent peers, but
  the test setup uses one `Database` connection. A two-`Database`
  scenario (two pools, two `claim_orphan_wake_for_recovery` calls
  truly in flight) would require either a tokio-style multi-thread
  runtime or two `Database` handles cooperating via `compio::spawn`,
  neither of which the suite currently does.
- **Why MINOR not IMPORTANT**: r19-C1's claim UPDATE is `UPDATE …
  WHERE state IN (non-terminal) AND lessee_updated_at < $1` — a
  single-pass `UPDATE` with implicit row-lock. The single-controller
  test correctly pins that "exactly one row gets claimed when run
  twice." The unmocked production multi-controller story relies on
  pg's row-lock contract, which is *not* what the test layer should
  pin (it's a pg-correctness invariant).
- **Action**: out-of-scope for unit tests; CALL OUT in retrospective
  doc — multi-replica behaviour validated empirically (smoke /
  staging) not by lib tests. Acceptable gap. Document in
  `docs/reference/sandbox-wake-jobs.md` if it doesn't already
  contain a "Why we don't test this" footnote.

## Carry-forward from r18

- **R18-T5** persist=Some chain — STILL OPEN (1 hit on `persist:
  Some` in pg_e2e vs. 1 on `persist: None`; the entire
  wake_machine_e2e suite uses the test-fixture path that skips
  persistence). 11th round. CARRY.
- **R18-T7** sibling probe loops — partially closed. Two probe
  loops covered (`wait_for_agent_silent`: 5 tests; `wait_for_agent_
  livez`: 4 R19-I1 tests). `restore.rs:570 probe_and_classify`
  still has 0 lib tests; `restore.rs` total is 2 tests (unchanged
  from r18). CARRY MINOR.
- **R18-T8** containerised pg fixture — STILL OPEN. `crates/
  sandbox/scripts/` contains 8 scripts; none is `pg-test-up.sh`.
  `.github/workflows/ci.yml` has zero mentions of `sandbox_pg`,
  `SANDBOX_TEST_PG`, or `--ignored`. The 87 pg-gated tests run
  ONLY on a dev box. CARRY MINOR; severity unchanged.
- **R18-T9** OS-thread detach pattern — gained one more site
  (`sweep.rs:440 detach_isolated("wake-takeover", …)` for R19-C1),
  so now FOUR sites use the detach pattern with zero direct lib
  coverage. CARRY MINOR.

## Standing recommendations (smoke-r13 retrospective adoption)

Per architecture-r19's three retrospective recommendations:

1. **"Premise-check invariant" template** — NOT adopted in test
   files or test-suite-level documentation. No `crates/sandbox/
   tests/` file carries a header comment articulating the
   premise-check pattern. Recommend: top-of-file block in
   `sandbox_pg_e2e.rs` listing the load-bearing premises for the
   wake_machine_e2e suite (e.g. "this fixture assumes lessee_updated_
   at is fresh on insert; verify before drive").
2. **"Phase-level histograms"** — NOT asserted in lib tests
   (cardinality, not value). No metric assertions for per-phase
   wake_machine state-transitions. R19-T3 above is a direct
   instance of this gap (claim count not metered).
3. **"Refuse doc-as-model"** — review template not yet updated.
   Recommendation: add a one-line item to the next test-coverage
   review template ("does any test assert against documented
   constants rather than the actual config object?").

These are review-process gaps, not code gaps. Surface them in r20
hand-off so the next cycle's reviewers adopt the new template.

## Cross-lens consensus

- **architecture-r19 R19-A1 / concurrency-r19 R19-C1** ↔ **test-cov
  r19 R19-T1**: the C1 takeover sweep is structurally CLOSED at the
  SQL layer (4 pg-gated) and config layer (2 lib); the missing tile
  is the wire-format response renderer. Unanimous: trivial 1-LOC
  add to existing 7-entry test.
- **architecture-r19 (probe wedge retrospective)** ↔ **test-cov
  r19 R18-T1-CLOSED + R19-I1 4 tests**: the two-phase probe pattern
  is now load-bearing tested. Both probe-loop sibling sites (`wait_
  for_agent_silent`, `wait_for_agent_livez`) cleared. The lesson
  from r18-T1 generalises in code (Phase 1 connect gate) but the
  test analogue — a `helpers::expect_probe_loop_completes_within`
  helper — was NOT extracted; tests duplicate the pattern.
  Refactor opportunity, not a gap.
- **code-quality-r19** ↔ **test-cov r19 R18-I1 closed**: 20 hits
  on `InsertWakeJobOutcome::Inserted` matches in pg_e2e (10 fixture
  sites × 2 occurrences each) — R18-I1 fixture hardening CLOSED at
  `531db5c3`. Cross-lens consensus.

## Lens hand-off

**To architecture r20**: R19-C1 cycle is structurally closed but
the operator-visible signal is missing — R19-T3 (no metric counter)
is architecture-adjacent. A claim-count counter is a 60s telemetry
signal that the wedge resolution is firing as designed; without it,
production has to grep tracing-warn logs.

**To code-quality r20**: R19-T1 is a 1-LOC fix that closes a wire-
format-coverage gap. Co-land with R19-T3's metric counter.

**To concurrency r20**: R19-T4 documents the multi-replica testing
limitation. Concurrency lens should sign off that single-controller
test + pg row-lock contract is acceptable, or call for a
two-`Database` fixture (would require a sidecar pg or an in-process
parallel-pool helper).

**To security r20**: R17-S1 sanitizer landed (6 tests cover 169.254/
16 IMDS + 100.64/10 CGNAT). The sanitizer is now load-bearing
covered for all RFC1918 + reserved ranges. No security follow-up
from test-cov this round.

**To test-cov r20**: backlog (~110 LOC total):
1. R19-T1 admin-handler wire code — ~1 LOC
2. R19-T2 `run_wake_jobs_takeover_once` lib unit tests — ~30 LOC
3. R19-T3 takeover claim-count metric + test — ~25 LOC
4. R18-T5 persist=Some chain (11th carry) — ~80 LOC
5. R18-T7 `probe_and_classify` lib test — ~30 LOC
6. R18-T8 containerised pg (out-of-band) — ~150 LOC
7. R18-T9 detach helper + 4-site tests — ~50 LOC

## Notes for r20

**Central question**: did r18's "probe-loop docstring without
wall-time test" lesson generalise to the next probe site
(`wait_for_agent_livez`)? **YES.** R19-I1 landed with 4 tests
covering the four shapes (happy / Phase-1-fail / late-bind /
Phase-1-OK-Phase-2-fail). The pattern is now load-bearing in code
and the test discipline followed without prompting.

**r19 severity baseline**: r18 had 1 CRITICAL (R18-T1) + 4 IMPORTANT
+ 3 MINOR. r19 has 0 CRITICAL + 1 IMPORTANT + 3 MINOR. The
emergency-de-escalation that started at r17 is now compounding —
r19 is the calmest round since r9.

**Cycle-defining pattern**: r19 = "the API + SQL surfaces of a new
sweep landed WITH 6 lib + 4 pg-gated tests; the wire-format
response renderer was the lone missed tile." For a 60s production
loop that wedges sandboxes when broken, the wire surface is
arguably the most important test. Future sweep landings: pin the
poll response in the same PR.

**Highest-leverage gap for r20**: R19-T1 (admin renderer wire-code
table). 1-LOC fix, closes wire-format-coverage gap on the freshly-
landed R19-C1 codepath. Co-land with R19-T3 metric.
