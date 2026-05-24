# Sandbox/snapshot-restore — test-coverage r22 review

Date: 2026-05-25 (UTC). HEAD at audit: `8b366b6d`. Round 22. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r21.md`
(HEAD `4d73a5d1`, baseline-compared at `8718120b`).

## Summary

- 5 findings (1 IMPORTANT + 4 MINOR) + 4 carries re-filed; 2 delists.
- Sandbox lib **428 → 432 (+4)**: R10-API4 agent-side wire-shape (+3,
  `sandbox-agent/src/handlers.rs:1117-1164`) + R22-I1 counter pin
  (+2, `metrics.rs:491-510`). r21-A1 was one assertion line on an
  existing test (extension, not new fn — r20-T2 over-fulfilment
  pattern carried into r22).
- pg-gated **87 → 89 (+2)**: R20-C1's `update_wake_job_state_
  after_ok_is_noop` + `..._after_failed_is_noop` (`sandbox_pg_e2e.rs:
  4934, 4999`). First pg movement since r19.
- **R20-C1 PR (`ccb2abc8`) finally co-lands lib+pg in one PR** —
  the "co-land" discipline test-cov flagged R20-T1/R21-T1 finally
  stuck. r17-Q3 looks like the last unit-only wedge.
- **R22-I1 already landed pre-audit** (`f98611fb`): 3 call sites
  in `wake_machine.rs` (`:139-147,184-192,521-528`) branch on
  `Ok(0)` → `tracing::warn!` + `inc_wake_terminal_overwrite_blocked
  ()`. 2 lib tests pin the counter. Code-quality r22 hand-off
  already resolved.
- **C-7-LT-10 strand: sandbox-side could NOT have caught via unit
  test** — see analysis below.

## Trend table

| Cycle | sandbox lib | pg-gated | Δ lib | Notes |
|-------|-------------|----------|-------|-------|
| r17   | 373         | 74       | +29   | PR1+PR2 |
| r18   | 402         | 83       | +29   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r19   | 424         | 87       | +22   | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| r20   | 426         | 87       | +2    | R19-I4 shims + R19-T1 widening |
| r21   | 428         | 87       | +2    | r17-Q3 DataIntegrity round-trip |
| **r22** | **432**   | **89**   | **+4**  | R10-API4 wire-shape + R20-C1 + R22-I1 counter |
| r9→r22 | +136       | +15      | —     | wedge + sweep + sanitizer + probe + retry + drift + envelope |

First round since r19 with movement on BOTH counters. PR
`ccb2abc8` (R20-C1) is the proof-point for the cross-round discipline.

## CRITICAL

(none.)

## IMPORTANT

### [R22-T1] Field-list parity contract test (R21-API2) still unimplemented — r21-A1 was the proof, r22 didn't ship the closure

- **Where**: cold-boot `nomad_ch.rs:2436-2462`; restore-path
  `restore_handler.rs:2344-2371`. Test pins are independent
  presence-lists (`nomad_ch.rs:4486-4519`,
  `restore_handler.rs:3586-3613`).
- **Symptom**: load-bearing invariant *"every Config field except
  `pubkey_hex` and `restore_from` must appear in BOTH builders"* is
  **not asserted anywhere**. r21-A1 is literal proof: `user_id`
  fell out of parity for one commit-cycle (`03d2f4a8` cold-boot,
  not restore). Both presence pins passed during that window. Only
  smoke-r17 caught it (cluster cost ~$0.50-1.00 + 2 cycle-rounds).
- **r21-A1's own deferred.md note (`:1809`) flags this as debt**.
  api-surface r21 hand-off (`r21.md:204-207`) named it. r22 did not
  address it.
- **Action**: ~30 LOC. Build both fixtures, extract Config keys via
  `as_object().keys().collect::<BTreeSet>()`, assert
  `cold ^ restore == {"pubkey_hex", "restore_from"}`. ONE test,
  catches every future `user_id`-shaped omission.
- **Severity IMPORTANT**: 2-round test-cov + api-surface
  convergence. Highest-leverage remaining work.

## MINOR

### [R22-T2] r21-A1 was extended via assertion-line patch — over-fulfilment pattern persists for the 3rd consecutive round

- **File**: `crates/sandbox/src/restore_handler.rs:3604-3612`.
- **Observation**: `ch_plugin_restore_jobspec_populates_restore_
  from` test grew from "asserts `restore_from` + `cpus` + `memory_mb`
  + `subnet_base_octet`" (r20 baseline) to "+ `user_id` (r21-A1)".
  The existing test's *name* says "populates_restore_from"; it now
  asserts 5 fields, not 1. The convention "one test name = one
  invariant pinned" is degrading. R20-T2 flagged this for the
  cold-boot pin; R21 carried it forward (C-7-LT-7); r22 adds a
  third instance.
- **Why MINOR**: tests still pass; nothing is incorrect. But a
  future engineer searching for "where is `user_id` Config-field
  emission pinned?" will not find a test of that name, and the
  field-list parity test (R22-T1) would supersede this in any case.
- **Action**: when R22-T1 lands, leave this test alone (it'll be
  redundant once parity is asserted symmetrically). If R22-T1
  slips a further round, factor `ch_plugin_restore_jobspec_emits_
  user_id_for_allow_list` as its own test. ~5 LOC.

### [R22-T3] R20-T1 retry-race pg-gated test STILL OPEN (3rd consecutive round)

- **Files**: `crates/sandbox/src/db.rs` `insert_wake_job` retry
  loop; should-be-tested at `crates/sandbox/tests/sandbox_pg_e2e.rs`
  `wake_jobs_crud::*`. No new pg-gated retry-race tests in r22.
- **State**: r20 spec'd the shape (two `#[compio::test]` tests:
  `insert_wake_job_retries_when_winner_terminal_within_select_window`
  + `insert_wake_job_returns_validation_after_3_conflicts_with_
  pending`). r21 carried unchanged. r22 same. R19-T3 metric
  add (counter for retry attempts) co-files.
- **Why still MINOR not IMPORTANT**: R20-C1's pg-gated tests
  (`update_wake_job_state_after_ok_is_noop`/`_failed_is_noop`)
  proved the "co-land lib + pg" discipline IS achievable; R20-T1
  is now the **only** open instance of the wedge-without-
  integration-test pattern. One more round of slip without a
  clear blocker and this escalates back to IMPORTANT at r23.
- **Action**: ~80 LOC pg-gated, co-file with R19-T3 counter +
  test. R23.

### [R22-T4] R18-T8 CI pg-gated plumbing — 4th consecutive round, zero movement; auto-delist as planned

- **State**: r21 marked "FINAL CARRY before delisting at r22 unless
  infra-lens picks it up." `.github/workflows/ci.yml` carries zero
  hits on `SANDBOX_TEST_PG`, `sandbox_pg`, `--ignored`.
  `crates/sandbox/scripts/` has no `pg-test-up.sh`. The 89
  pg-gated tests run only on dev boxes. No infra-lens pickup in
  r22 either.
- **Action**: **DELIST.** Test-cov lens cannot drive CI plumbing
  without infra-lens collaboration. If a future cycle introduces
  a non-dev-only target for pg-gated tests, this returns at
  IMPORTANT. Until then, the 89-test pg-gated suite is dev-
  validated-only by design.

### [R22-T5] R19-T4 multi-replica doc — 4th consecutive round; auto-delist as planned

- **State**: r21 marked "second-final round before delisting." r22
  brings no `docs/reference/sandbox-wake-jobs.md`. The proposed
  lower-effort alternative (append prose to `sweep.rs` doc-comment
  at `run_wake_jobs_takeover_once`) also did not land. Multi-
  controller takeover staging-validation note remains undocumented.
- **Action**: **DELIST.** Carry-forward fatigue. The behaviour is
  observable in `sweep.rs` source (run_wake_jobs_takeover_once is
  ~50 LOC and self-explanatory); the doc gap is process, not
  correctness. If a multi-replica deploy bug surfaces, this
  returns. Until then, code-as-doc suffices.

## Would-have-caught analysis for C-7-LT-10 strand

**Answer**: **source-audit was fundamentally needed**. C-7-LT-10 is
a **driver-side** bug: `nomad-driver-ch/ch/restore_task.go:319`
writes rewritten `config.json` to `<runDir>/config.json`, `:420`
passes `--restore source_url=file://<RestoreFrom>` to CH. Two
directories; CH reads the un-rewritten copy.

Sandbox-side comment at `restore_handler.rs:2316-2318` correctly
describes the driver contract (`--restore source_url=file://
<RestoreFrom>`). The comment is **accurate about what the driver
SHOULD do**; the bug is the driver's implementation diverging from
its own contract. No sandbox-side unit test could catch this — the
sandbox crate has no visibility into where `restore_task.go`
actually points CH's `source_url`. Sandbox pins its own emission at
`restore_handler.rs:3571`; it cannot pin what the driver does with
the field.

**Conclusion**: source-audit is the correct tool for cross-repo
contract divergence. r19-r21's three cycles of stderr-pattern-only
diagnosis are an architectural / cluster-review problem, not a
test-cov gap. **No test-cov action** for the C-7-LT-10 class beyond
R22-T1, which pins field PRESENCE but not driver path behaviour.

## Carry-forward from r21

- **R21-T1** r17-Q3 DataIntegrity integration test — OPEN.
  Downgraded MINOR (R20-T1 also slipped; pattern uniform).
- **R21-T2** host-fence observable constants — architecture r22
  did not rule. CARRY MINOR.
- **R21-T3** premise-check pattern — 4 rounds zero adoption.
  **DELIST.**
- **R18-T5** persist=Some chain — OPEN, 14th round. MINOR.
- **R18-T7** `probe_and_classify` sibling probe loops — OPEN.
  MINOR.
- **R18-T9** OS-thread detach pattern — 4 sites uncovered. MINOR.
- **R19-T2** `run_wake_jobs_takeover_once` lib coverage — OPEN.
  MINOR.
- **R19-T3** R19-C1 claim-count metric — OPEN; co-land R22-T3.
  MINOR.

## Cross-lens consensus

- **R22-T1 ↔ api-surface r21 R21-API2 ↔ code-quality r22 (lens
  hand-off #4)**: three lenses converge on the field-list-parity
  contract test. api-surface flags it as the regression-vector
  closure; code-quality flags it as the test-cov hand-off; test-cov
  flags it as the IMPORTANT this round. Convergence signal:
  ship it.
- **R22-I1 already landed** (code-quality r22 IMPORTANT): the
  tracing+counter at `wake_machine.rs:139-147,184-192,521-528` plus
  2 lib tests resolves code-quality's only IMPORTANT. Cross-lens:
  CLOSED before the audit point.
- **R20-T1 ↔ R21-T1 ↔ R22-T3** (test-cov internal, 3-round
  pattern): pg-gated retry-race + DataIntegrity integration tests
  continue to slip. R20-C1's co-land discipline (`ccb2abc8`)
  proves the pattern is achievable; the open items are a
  prioritisation gap, not a structural one.
- **C-7-LT-10 source-audit** (cluster lens): no test-cov action.
  Cross-repo contract divergence is architecture / driver-lens
  territory. Sandbox-side did everything right.

## Lens hand-off

**To api-surface r22**: R22-T1 closes R21-API2. Recommended PR
title "field-list parity contract test (R21-API2 / R22-T1)".
Single ~30 LOC test in `restore_handler.rs` tests module.

**To concurrency r22**: R22-T3 (R20-T1 carry) — retry-race
pg-gated test spec is now 3 rounds old. Concurrency lens to
validate the race-window choreography before authoring (or
declare it overspec'd and downgrade to MINOR permanently).

**To architecture r22**: R21-T2 (host-fence constants) still
unjudged. Either rule on `pub(crate) const` promotion or test-cov
delists at r23.

**To test-cov r23 backlog** (~145 LOC total, down from r21's 300):
1. R22-T1 field-list parity contract test — ~30 LOC (NEW IMPORTANT)
2. R21-T1 pg-gated DataIntegrity test — ~50 LOC
3. R22-T3 R20-T1 retry-race pg tests — ~80 LOC (if concurrency r22
   greenlights)
4. R19-T3 takeover claim-count metric + test — ~25 LOC (co-land
   with R22-T3)

Delisted this round: R18-T8 (CI plumbing), R19-T4 (multi-replica
doc), R21-T3 (premise blocks). Backlog shrinking — wedge is
saturated, remaining work is depth + cross-lens coordination.

## Notes for r22

- **Co-land lib + pg discipline FINALLY stuck** via R20-C1
  (`ccb2abc8`): +2 lib + +2 pg-gated, same PR. r17-Q3 was the last
  unit-only wedge landing. R22-T1 is the next test for the
  discipline.
- **r22 sidestepped the `pub(crate)`-input problem** by making the
  *observable* (`rows_affected == 0` + counter) the test target,
  not the input. Cleaner than mocking inputs. Architecture r22 did
  not rule on the broader question.
- **Highest-leverage closure for r23**: R22-T1, ~30-LOC PR.
  Closes R21-API2 + R22-T1 + r21-A1 debt note in one move.
