# Sandbox/snapshot-restore — test-coverage r16 review

Date: 2026-05-25 (UTC)
HEAD at audit: `792a7aa5` (pilot artifact commit; latest production-code commit is `da951dd9` A1-FOLLOWUP).
Round 16 of N (test-coverage lens). Catching up from r15 (`b8fae7b7`).
Prior: `docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r15.md`.

## Summary

- 5 NEW findings (1 CRITICAL holding at EMERGENCY, 2 IMPORTANT, 1 MINOR, 1 POSITIVE/MINOR) + 9 carry-forwards re-filed.
- Test counts at HEAD `792a7aa5` by `cargo test -p zeroship-sandbox --lib`: **344 passed / 1 ignored / 0 failed** (was **332** at r15 HEAD `b8fae7b7`). **+12 sandbox-lib tests this cycle (+1 C-8b at `64af1803`, +6 A1-FOLLOWUP at `da951dd9`, +5 unaccounted — likely intermediate C-8/C-8a additions at `2afbb2dd`).** Net delta is POSITIVE this cycle, breaking the r15 flatline.
- **Integration tests in `crates/sandbox/tests/` and `crates/sandbox-agent/tests/`: zero new tests for the 7th consecutive cycle.** Verified at HEAD `792a7aa5`: `git log --oneline b8fae7b7..792a7aa5 -- crates/sandbox/tests crates/sandbox-agent/tests` returns empty.
- **`StubRestoreBackend::fail_submit` and `::fail_livez` remain DECLARED-BUT-UNUSED across the entire workspace** — 8th consecutive round. Grep at `crates/sandbox/src/restore_handler.rs:1168-1169` (declared) vs. ZERO callsites in tests or source. Only `fail_reserve` is exercised, and only at `tests/sandbox_pg_e2e.rs:2781`. The 7-round CRITICAL carry (R10-T1 → R15-T1 → R16-T1) is still the audit's largest open gap.
- **Cluster cycle r9, r10, r11 added 3 more bugs (C-8, C-8a, C-8b confirmed but C-8c NEW)** — bug count is now **C-1 … C-8c = 10+ distinct production-only signals across 11 cluster cycles**. The r11 reviewer concluded: **"there is no remaining knob inside the synchronous-response contract that fixes this without violating the deadline"** (`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-r11.md` TL;DR). Two cycles in a row (C-8b at `64af1803`, A1-FOLLOWUP at `da951dd9`) DID add tests this round, but ZERO of them exercise the actual wedge surface (wake handler end-to-end + future drop) — C-8b's tests are still constant-arithmetic on the policy formula; A1-FOLLOWUP's tests are config-validation, orthogonal to wake.
- The architecture-r16 lens (`docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r16.md` r16-A1) names C-8b's `2 × fence` constant as a **numeric coincidence, not a model** — the formula is correct only at the single fence value (30 s) that smoke-r10 measured. None of the +1 C-8b unit tests (`c8b_default_policy_envelopes_doubled_fence` at `restore_handler.rs:1649-1684`) drive the formula against a property the architecture review names (Nomad purge tail's independent 30 s timeout, fence < 30 regime); they pin the magic number at one specific input. This is **R15-T2's "test-the-constant-not-the-behaviour" defect class continuing into round 16** under a new name (R16-T2).
- The C-7-LT context (per brief: "PR1 will add 4-6 pg-gated tests for wake_jobs CRUD; PR2 will add unit tests for the state machine"): the wake_jobs table + state machine DO NOT EXIST in the codebase today — grep for `wake_jobs` returns zero matches under `crates/sandbox/`. PR1+PR2 will close R10-T1 / R10-T2 only if their pg-gated fixtures wire `StubRestoreBackend` through `restore_sandbox` with the per-PR1 wake_jobs row as state-of-record. **None of the +12 tests landed this cycle make progress toward that closure.**

## Trend table (8-cycle test count history, updated)

| Cycle | sandbox lib | sandbox-agent lib | Δ sandbox | Δ sandbox-agent | Notes |
|-------|-------------|-------------------|-----------|-----------------|-------|
| r9    | 296         | 231               | —         | —               | baseline |
| r10   | 303         | 239               | +7        | +8              | R7-S1, R8-A2 etc. |
| r11   | 310         | 240               | +7        | +1              | R7-API2 + R10-Q1 etc. |
| r12   | 327         | 242               | +17       | +2              | R12-I1 + carries |
| r13   | 328         | 242               | +1        | 0               | C-3 regression test |
| r14   | 332         | 242               | +4        | 0               | C-4 retry harness |
| r15   | 332         | 242               | 0         | 0               | C-6 (+0) + C-7 (+1/-1) |
| **r16** | **344**   | **242** (unverified — assume unchanged; no commits touched `sandbox-agent/src/**` since b8fae7b7) | **+12** | **0** | C-8/C-8a (+~5 inferred at `2afbb2dd`) + C-8b (+1 at `64af1803`) + A1-FOLLOWUP (+6 at `da951dd9`) |
| **r9 → r16 total** | **+48** | **+11**  | —         | —               | 9-round growth; integration test delta still **0** |

**+12 is the largest single-cycle sandbox-lib delta since r12.** This is the FIRST cycle since r12 to break the post-r12 stagnation curve. However: the additions are concentrated in (a) policy-formula constant-arithmetic (C-8/C-8a/C-8b), and (b) config-validation table-tests (A1-FOLLOWUP). Neither touches `restore_sandbox` end-to-end nor exercises the `fail_submit` / `fail_livez` stub branches. **The integration-test column remains at 0/0/0 for the 7th cycle.**

## Findings (NEW since r15)

### [R16-T1] EMERGENCY HOLD — 8-cycle integration-test gap; `StubRestoreBackend::fail_submit` + `::fail_livez` STILL declared-but-unused; +12 unit-test additions left the gap untouched (EMERGENCY, test-coverage-r16, re-filed from R15-T1)

- **Files**: `crates/sandbox/src/restore_handler.rs:1168-1169` (`pub fail_submit: bool` / `pub fail_livez: bool` declared); `crates/sandbox/src/restore_handler.rs:1258`, `:1266` (sole production callsites — `if self.fail_submit { … }` / `if self.fail_livez { … }`, both inside `impl RestoreBackend for StubRestoreBackend`); `crates/sandbox/tests/sandbox_pg_e2e.rs:2781` (the **only** site setting any stub-failure flag — `backend_inner.fail_reserve = true`; ZERO sites set `fail_submit=true` OR `fail_livez=true`); `crates/sandbox/src/restore_handler.rs:498` (`pub async fn restore_sandbox` — the integration entry, called only 5× from `tests/sandbox_pg_e2e.rs:2707, 2757, 2785, 3237, 3359`, **all with `persist=None`**); `crates/sandbox/src/admin_handlers.rs:1339-1392` (the C-6 OS-thread detach pattern — STILL byte-coverage-zero, 6th consecutive cycle); `crates/sandbox/src/restore_handler.rs:710` (`async fn do_restore_inner` — the full wake-path orchestrator; the only async caller of all four `RestoreBackend` methods; **NO test calls this function with stubbed submit/livez failures**).
- **Symptom**: r15 closed at EMERGENCY with the central question "after 9 cluster cycles and 7 bugs, has anything closed the structural test-coverage gap?" → answer NO. r16's answer: **still NO, but the cycle's +12 unit tests created an illusion of motion.** The 6 A1-FOLLOWUP tests at `crates/sandbox/src/lib.rs:2349-2447` are excellent (PROPER table-test, behavioural drive of `assert_kek_required_for_remote_store`, 6 input combinations covering the truth table). The +1 C-8b test at `:1649-1684` is constant-arithmetic-only (R16-T2 below). But ZERO of the +12 tests exercise the EMERGENCY gap: stub failure flags through `restore_sandbox` end-to-end. Smoke cycles r9, r10, r11 added C-8, C-8a, C-8b CONFIRMED + C-8c NEW — the 1-cycle-1-bug pattern is **9 cycles deep, 10+ distinct bugs (C-1 … C-8c)**. Smoke-r11's TL;DR is the definitive admission: *"there is no remaining knob inside the synchronous-response contract that fixes this without violating the deadline. C-7-LT (async wake response + polling, R15-A1) is the only path forward."* So the test-coverage gap is now downstream of an architectural gap; pre-C-7-LT, the audit's #1 ask (pg-e2e fixture with `fail_submit=true`) is still relevant because the OS-thread detach pattern (R15-T1b) and the rollback path (R10-T1) live OUTSIDE the C-7-LT migration.
- **Why it matters**: the C-7-LT PR1 (per brief, 4-6 pg-gated tests for `wake_jobs` CRUD) is the structural fix; the audit's R15-T1 #1 is the IMMEDIATE coverage that prevents another C-N being missed before PR1 lands. The four sub-asks from r15 remain open:
  - **#1 (pg-e2e `fail_submit`)**: no fixture exists. The `fail_submit` field is dead code, has been since R7 (round of introduction).
  - **#2 (pg-e2e `persist=Some(_)`)**: no fixture exists. The unseal → register_restored → clock_resync chain inside `do_restore_inner` has zero pg-e2e drive.
  - **#3 (future-drop unit test wrapping `reserve_vm_index_with_retry`)**: no test exists. The C-7 fix at `493d6c1e` shipped an arithmetic-only assertion; the C-7-LT structural fix (per brief) will REPLACE the synchronous-response shape entirely, but pre-C-7-LT the loop's future-cancellation behaviour still needs pinning.
  - **#4 (OS-thread detach extraction + unit test)**: the C-6 OS-thread detach pattern at `admin_handlers.rs:1339-1392` is byte-coverage-zero for the **6th consecutive cycle**. The R15-Q1 refactor at `7469118e` changed the thread-name builder (byte-slice tail extraction) but added zero test — verified: `git show 7469118e --stat -- crates/sandbox/src` shows `crates/sandbox/src/admin_handlers.rs | 33 ++++++++--`, no test file touched.
- **Action**: **HOLD AT EMERGENCY.** Eight rounds; the per-cycle pattern is "fixes ship a numeric or table-test pin near the regression point, but no fixture drives the wedge surface through the stub harness." The r16 surface area now ADDS C-8c (smoke-r11) and the architecture-r16 r16-A1 finding (the `2 × fence` constant is a coincidence at fence=30 only). **Recommended pilot directive: pin C-7-LT-PR1 as a HARD prerequisite for sprint closure** — but the four sub-asks above are STILL the minimum coverage that prevents new C-N bugs during the C-7-LT migration. **Specifically**: the rollback path at `restore_handler.rs:601-689` (the `target = Snapshotted / SnapshottedSuspect` branch + the spawn_blocking-wrapped teardown at `:625-633`) is what `fail_submit=true` would exercise; this is ORTHOGONAL to wake_jobs state-machine work and would compose with PR1+PR2 cleanly.

### [R16-T2] R15-T2's "test-the-constant-not-the-behaviour" defect class continues into r16 — C-8b's `c8b_default_policy_envelopes_doubled_fence` pins ONE input (fence=30) where the formula is correct by construction; the architecture-r16 r16-A1 review names this as the formula's only valid input point (IMPORTANT, test-coverage-r16, NEW)

- **Files**: `crates/sandbox/src/restore_handler.rs:1572-1684` (4 `r14a6_*` + `c8b_*` tests, all `#[test]` constant-arithmetic on `VmIndexRetryPolicy::from_host_fence_timeout(N)` for N ∈ {20, 60, 120, 0, 30}); `docs/reviews/sandbox-snapshot-restore-architecture-2026-05-25-r16.md:13-22` (r16-A1 explicitly: *"The formula is correct ONLY at the single fence value (30 s) the smoke happened to use."*); commit `64af1803` (C-8b fix, adds `c8b_default_policy_envelopes_doubled_fence` at `:1649-1684`).
- **Symptom**: r15-T2 flagged `c7_retry_budget_default_is_under_client_deadline` as repeating r14-T2's defect class (constant comparator masquerading as a behavioural test). The C-8b fix at `64af1803` added one more `#[test] fn` of the same shape:
  ```rust
  fn c8b_default_policy_envelopes_doubled_fence() {
      let p = VmIndexRetryPolicy::from_host_fence_timeout(30);
      let wall_ms = p.interval.as_millis() as u64 * u64::from(p.max_attempts.saturating_sub(1));
      assert!(p.max_attempts >= 21, …);
      assert!(wall_ms <= 50_000, …);
      assert_eq!(p.max_attempts, 26, "C-8b: 30 s fence → (2*30 - 10) / 2 + 1 = 26 attempts; got {}", …);
  }
  ```
  The arch-r16 r16-A1 finding is the second-order receipt: this formula is **correct only when fence=30 AND Nomad purge tail ≈ 30 s by coincidence**. Under fence ∈ {0..15} the formula under-estimates teardown by ~2× (Nomad's independent 30 s purge tail dominates); under fence ≥ 60, the deadline-ceiling MIN-of-two binds and the 2× factor is inert (the test passes because the deadline-ceiling, not the fence-ceiling, drives `max_attempts=26`). So the test pins a one-point measurement, not a model. None of the 5 `r14a6_*` + `c8b_*` tests at `:1530-1684` exercise the under-budgeted regime (fence < 30) where the architectural property the formula is supposed to encode ("teardown bounded by fence + nomad_purge + cleanup") would surface as a budget shortfall.
- **Why it matters**: r14-T2 was IMPORTANT-tracking; r15-T2 receipted it (`c4_default_policy_envelopes_observed_teardown` was deleted because constants moved); r16-T2 receipts it AGAIN (the architecture lens names `c8b_*` as one-point-only). **Three consecutive rounds the same defect class has surfaced.** The C-7-LT migration (per brief, PR2 will add unit tests for the state machine) is the structural fix — once the synchronous-response contract is gone, `from_host_fence_timeout` is moot. But pre-C-7-LT, the next operator who tunes `host_fence_timeout_secs` to a non-30 value will discover a budget shortfall the unit tests don't catch.
- **Action**: rewrite the 5-test family as **property tests over (fence, nomad_purge_timeout, deadline) tuples**, asserting (a) derived budget ≥ teardown_estimate − HEADROOM, (b) derived budget ≤ CLIENT_DEADLINE − HEADROOM, (c) the model decomposition matches `wait_for_agent_silent` + `wait_for_job_gone` + cleanup-tail (per r16-A1's fix sketch). 30–40 LOC. Tracking-only this round; the C-8b fix IS correct at the cluster-smoke configuration (fence=30), the test just doesn't pin the property the formula is supposed to encode.

### [R16-T3] A1-FOLLOWUP's +6 tests at `lib.rs:2349-2447` are the cycle's ONE example of behavioural drive-through-pure-function pattern — but the analogous `assert_persist_required_when_snapshot_enabled` already had this pattern, so the cycle delta is "we caught one missing fail-CLOSED check," not "we built new test infrastructure" (MINOR + POSITIVE, test-coverage-r16, NEW)

- **Files**: `crates/sandbox/src/lib.rs:975-1004` (`pub(crate) fn assert_kek_required_for_remote_store` — pure validation fn, returns `Result<(), String>`); `crates/sandbox/src/lib.rs:2349-2447` (the 6 new tests + `mod` opener); `crates/sandbox/src/lib.rs:2282-2336` (the 5-test sibling for `assert_persist_required_when_snapshot_enabled` — the pattern this cycle mirrored); commit `da951dd9` (A1-FOLLOWUP body: *"6 new unit tests pin the truth table: tiered+GCS + KEK set → ok, tiered+GCS + KEK unset → Err, L1-only + KEK set → ok, L1-only + KEK unset → ok, snapshot_enabled=false → ok across all 4 use_gcs/kek combos, tiered+GCS + KEK unset + test override → ok"*).
- **Why it's a POSITIVE**: this is the audit's preferred shape — pure-fn extraction + truth-table tests. The 6 tests cover (a) the happy production path, (b) the FATAL Err path, (c) two ergonomics-preserving paths (L1-only with/without KEK), (d) the test-override escape hatch, (e) the "feature off" no-op. **THIS is what the audit has been asking for since r10.** Six tests, six combinations, every flag flipped at least once, zero magic numbers in the body, behavioural drive of the function-under-test.
- **Why it's still only MINOR (not a sweep-the-board win)**: the pattern was already in place at `:2282-2336` — the cycle's contribution is recognising that A1's silent-degrade-on-missing-KEK gap matched the R5-S1/B21 pattern and applying it. **None of the other open gaps got this treatment.** The C-6 OS-thread detach pattern is still byte-coverage-zero; the rollback path's `target = SnapshottedSuspect` branch at `restore_handler.rs:601-605` is still unreached; the `fail_submit`/`fail_livez` flags are still declared-but-unused. The +6 A1-FOLLOWUP tests are a model of how to close a single gap, not a sweep.
- **Action**: **CARRY FORWARD AS PATTERN.** Future fail-CLOSED additions (and there will be more — the audit predicts at least one more from the C-7-LT migration around `wake_jobs.state` transitions) should mirror `assert_kek_required_for_remote_store_tests` line-for-line. Tracking-only; no immediate action required.

### [R16-T4] The `+5 unaccounted unit tests` between r15 (332) and r16 (344) — attributable to the C-8 + C-8a commit at `2afbb2dd` — landed without test-coverage-r15 review hand-off and need post-hoc audit (MINOR, test-coverage-r16, NEW)

- **Files**: commit `2afbb2dd` ("sandbox: cap retry budget at client deadline (C-8a) + 30s host_fence for cluster (C-8)") — `git show 2afbb2dd --stat` shows: `crates/sandbox/src/restore_handler.rs | 285 ++++++++++++++++++++++++++++--`, no separate test file modified.
- **What I see**: r15 baseline 332. r16 HEAD 344. Known additions: +1 (C-8b at `64af1803`) + +6 (A1-FOLLOWUP at `da951dd9`) = +7. Unaccounted: +5. Inferring from the `restore_handler.rs +285 / -28` stat at `2afbb2dd` and the C-8a commit body's claim of "structural fix: cap budget at client deadline," the most likely additions are the 4 `r14a6_*` tests at `:1530-1632` (`r14a6_policy_from_cfg_respects_host_fence_timeout`, `_short_timeout`, `_zero_fence_still_attempts_once`, `r14a6_from_cfg_caps_at_client_deadline`) + the inherited C-7 test name change. **These tests went through the audit's review cycle WITHOUT a test-coverage-r16 review having been written yet** (r15 audited at `b8fae7b7`, this is the FIRST round to see `2afbb2dd`'s additions). On post-hoc inspection all 4 are constant-arithmetic-only (per R16-T2's defect class).
- **Why it's a MINOR**: the audit's normal cadence is per-cycle review of per-cycle commit batches; r15-round-2 (`d392a308`) bundled `2afbb2dd`'s closures BEFORE this test-cov round saw the additions. The +5 tests ARE in the cycle's contribution graph, but their defect-class footprint is fully within R16-T2's coverage. Tracking-only — flagging this gap in the audit's review-batching so the next time multiple bug-fixes land between rounds, the test-cov reviewer sees them all in one round rather than spread across two.
- **Action**: re-baseline the trend table (done above — `+12` row for r16). No reviewer action needed beyond noting the catch-up.

### [R16-T5] C-7-LT-PR1+PR2 GAP MAP — the brief names "4-6 pg-gated tests for wake_jobs CRUD" + "unit tests for the state machine" as in-flight; mapping that against r16's open gaps shows PR1+PR2 will close R10-T1/T2 ONLY IF the pg fixtures wire `StubRestoreBackend` through `restore_sandbox` (IMPORTANT, test-coverage-r16, NEW — forward-looking)

- **Files**: NO `wake_jobs` table OR `WakeJob` type exists in the codebase today — `grep -rn 'wake_jobs\|WakeJob\b' crates/sandbox/src crates/sandbox/tests crates/control/src` returns ZERO matches at HEAD `792a7aa5`. The C-7-LT design surface lives in deferred-review entries + architecture-r15 R15-A1, NOT yet in code.
- **What PR1 should add (per brief — "4-6 pg-gated tests for wake_jobs CRUD")**:
  1. `wake_jobs` schema migration + CRUD wrappers in `crates/sandbox/src/db.rs`.
  2. `create_wake_job(sandbox_id, started_at) → wake_job_id` pg-gated test.
  3. `get_wake_job(wake_job_id) → Option<WakeJob>` pg-gated test.
  4. `update_wake_job_state(id, from, to, generation) → Result<u64>` pg-gated test (CAS).
  5. `list_active_wake_jobs() → Vec<WakeJob>` pg-gated test.
  6. (Optional) sweep takeover test for wake_jobs in the existing `crates/sandbox/src/sweep.rs` pattern.
- **What PR2 should add (per brief — "unit tests for the state machine")**: pure-fn `WakeJobState::next(current, event) → Result<WakeJobState>` truth-table tests, mirroring the A1-FOLLOWUP pattern at `lib.rs:2349-2447` (~8-12 tests covering all valid transitions + 3-5 invalid).
- **GAP analysis**: PR1+PR2 close R10-T2 (`Persist(Some(_))` chain visibility) ONLY if the pg-gated tests REPLACE the synchronous-response wake path entry — i.e., the test wires `POST /admin/wake → enqueue wake_job → poll wake_job until terminal → assert row state` with `StubRestoreBackend { fail_submit=true }` injected into the controller's `do_restore_inner` worker. Without that wiring, PR1+PR2 cover only the new wake_jobs CRUD surface; R10-T1 (the `fail_submit` end-to-end rollback assertion) remains open because the existing `restore_sandbox` synchronous path is what carries the rollback semantics.
- **Concrete prereq backlog** (in dependency order):
  1. **Pre-PR1**: extract `do_restore_inner` to accept `&dyn RestoreBackend` injection in a pg-gated test (~30 LOC, no schema changes). This is the R15-T1 #1 ask — independent of C-7-LT.
  2. **PR1 task 6**: add to the PR1 set a pg-gated test that creates a wake_job, drives it through the new async worker with `StubRestoreBackend { fail_submit=true }`, asserts the wake_job row terminates with `state='failed'` and `error_class='backend_submit'`, and the underlying sandbox row reverts to `snapshotted` with `generation=g0+2`.
  3. **PR2 task N+1**: add a state-machine test for the rollback CAS that handles "wake_job in progress; controller crashes; sweep takeover finds the in-flight row and rolls back" — the sweep takeover side IS already open as R9-T8.
- **80-LOC pg-e2e fixture sketch** (the R15-T1 #1 ask, achievable independently of C-7-LT):
  ```rust
  // crates/sandbox/tests/sandbox_pg_e2e.rs (new test)
  #[compio::test]
  async fn wake_fails_on_submit_returns_503_and_reverts_row() {
      let (db, _pg) = setup_pg_test_db().await;
      let sid = insert_snapshotted_row(&db, vm_index=7).await;
      let store = Arc::new(local_snapshot_store_fixture());
      let mut stub = StubRestoreBackend::new(tempdir());
      stub.fail_submit = true; // ← the dead-code branch this fixture wakes up
      let backend = Arc::new(stub);
      let outcome = restore_sandbox(&db, store, backend.clone(), None, sid, true).await;
      assert!(matches!(outcome, Err(RestoreHandlerError::Backend(_))));
      let row = db.get_sandbox_row(sid).await.unwrap().unwrap();
      assert_eq!(row.status, SandboxStatus::Snapshotted, "row must revert");
      assert_eq!(row.generation, /* g0 + 2 */); // CAS-restoring then CAS-snapshotted
      assert!(backend.submit_called.load(Ordering::SeqCst));
      assert!(!backend.livez_called.load(Ordering::SeqCst)); // never reached past submit
      assert_eq!(backend.released.lock().unwrap().as_slice(), &[7]); // rollback teardown released slot
  }
  ```
  Mirror with `fail_livez=true` (second 80 LOC) → ~160 LOC closes BOTH `fail_submit` AND `fail_livez` end-to-end and pins the rollback contract pre-C-7-LT.
- **Action**: **PIN AS HARD PREREQUISITE FOR R17.** R15-T1 #1 + this sketch is the minimum-deliverable backlog. If PR1+PR2 land WITHOUT this fixture, R10-T1 stays open into round 17 and we'll be at 8 cycles of EMERGENCY with the structural fix landed (C-7-LT) but the pre-existing rollback path still unverified.

## Smoke-cycle catch list (per brief — would a stub-backend harness have caught it?)

| Cycle | Bug | Symptom | Stub-catchable? | Why |
|-------|-----|---------|-----------------|-----|
| r4 | C-1 (CAS predicate matches other controllers' wedges, not self) | Sweep CAS not idempotent across controllers | **NO** | Multi-controller distributed property; needs ≥2 controller processes. The C1-FOLLOWUP at `0e71e5c4` is fixed in pg-gated tests but with a single-controller harness it would have slipped. |
| r4 | C-2 (Go driver `Cannot open disk path`) | Driver materialization race | **NO** | Lives in Go driver, not Rust. Stub harness has no scope. |
| r5 | C-3 (`spawn_blocking` panic inside `TieredSnapshotStore::put`) | Compio runtime panic on detached spawn_blocking | **YES** | Was directly testable; `r13` added one regression test at `snapshot_store_gcs.rs:1405-1439` AFTER the fact. A pre-C-3 fixture wrapping the detached future would have caught it. |
| r5 | C-4 (immediate 503 from `reserve_vm_index` racing source-teardown) | Wake arrives ms after snapshot, slot still held | **YES** | The C-4 retry-loop tests at `restore_handler.rs:1374-1479` ARE this pattern, but landed AFTER cluster smoke caught it. A pre-existing `fail_reserve=true, reserve_succeeds_on_attempt=Some(N)` fixture wired through `restore_sandbox` end-to-end would have caught the missing retry loop. |
| r6 | C-5 (state-map insert misses on register_restored) | Pre-B19 wake left the restored VM invisible to backend registry | **MAYBE** | The B19 fix has a unit test at `restore_handler.rs:3115-3220` ("Reserve + register: mirrors the do_restore_inner success path"); stub-harness with a fake `nomad_handle` is plausible but the actual bug was in cross-crate composition (`AppState::from_config` wiring). |
| r7 | C-6 (compio runtime starvation by detached teardown) | Working hypothesis — **FALSIFIED in r8** | **NO** | The C-6 hypothesis was falsified by r8; the actual symptom (silent wedge at `pre_reserve_vm_index`) was C-7. A C-6-shaped stub test would have pinned the wrong hypothesis. |
| r8 | C-7 (ntex drops wake handler future on client disconnect at 60s) | Wake handler future cancelled mid-`compio::time::sleep.await` | **YES** | This IS testable: wrap `reserve_vm_index_with_retry` in `compio::time::timeout(Duration::from_millis(80), ...)` against `fail_reserve=true`, assert clean cancellation + no orphan reservation. R15-T1 #3 = this exact ask. Still not done. |
| r9 | C-8 (host_fence env var not propagated to driver) | Cluster used 5s default fence vs. 30s configured | **NO** | Configuration-propagation bug across Nomad job spec + driver env block; stub harness has no Nomad/driver. |
| r9 | C-8a (deadline-cap missing from `from_host_fence_timeout`) | Fence=120 → budget=110s > 60s deadline; C-7 silent-cancel regression | **YES** | The `r14a6_from_cfg_caps_at_client_deadline` test at `:1609-1632` IS this pattern, but is constant-arithmetic-only (R16-T2). A property-test over (fence, deadline) tuples would have caught the missing MIN earlier. |
| r10 | C-8b (`2 × fence` ceiling needed) | fence=30 → budget=20s but teardown=60.164s; under-budgeted | **PARTIALLY** | The 60.164s teardown is composed of `wait_for_agent_silent` (~30s) + Nomad purge tail (~30s) — both have **separate** timeouts in code. A model-based test pinning teardown = fence + nomad_purge_timeout + cleanup would have caught this, but only IF the test wired in the Nomad-side timeout constant (which lives in `nomad_ch.rs:1051`, separate from `restore_handler.rs`). The pure-fn test isolated to `from_host_fence_timeout` would NOT have caught it. |
| r11 | C-8c (the 50s deadline-ceiling itself is too tight) | Teardown 60.166s > 50s capped budget; **no remaining knob** in synchronous contract | **NO** | The smoke-r11 reviewer's verdict: this is structural — only C-7-LT (async wake) closes it. Stub harness can't catch a deadline that's correctly enforced by ntex. |

**Stub-catchable tally for r4-r11 (the brief's question)**: **YES on C-3, C-4, C-7, C-8a (4 of 10 distinct bugs).** PARTIALLY: C-8b (1). MAYBE: C-5 (1). NO: C-1, C-2, C-6 (falsified hypothesis), C-8, C-8c (5). **Total "stub-catchable" = 4 / 10 (40%) clean YES; 5 / 10 (50%) including PARTIALLY + MAYBE.** Four bugs that the audit could have caught BEFORE cluster smoke if R10-T1's pg-e2e fixture had landed in r10; instead they were caught by cluster smoke at a cost of ~1 cycle each (~4 cycles of smoke time).

## Closed by recent commits (since r15 audit at `b8fae7b7`)

- **C-8b fix** at `64af1803` (`from_host_fence_timeout` 2× factor; **+1 test `c8b_default_policy_envelopes_doubled_fence` + 1 test updated** `r14a6_policy_from_cfg_short_timeout`).
- **C-8b closure** at `0fc56df5` (doc-only).
- **R15-Q1 (admin_handlers byte-slice tail extraction for `zsbx-teardown` thread name)** at `7469118e` (no test added; the R15-T1b OS-thread detach coverage gap is unchanged).
- **Controller pin bumps** at `c73b3956` (v24 → v25) and `2e9ae598` (v25 → v26) (script-only).
- **Smoke-r10 review** at `0c213a5a` (doc-only — C-8b NEW).
- **Smoke-r11 review** at `3fc192b2` (doc-only — RED, C-8c NEW).
- **A1-FOLLOWUP fix** at `da951dd9` (`assert_kek_required_for_remote_store` fail-CLOSED; **+6 tests at `lib.rs:2349-2447`** — see R16-T3).
- **A1-FOLLOWUP closure** at `a1da96cc` (doc-only).
- **Pilot artifact bundle** at `792a7aa5` (r16 reviewer artifacts — doc-only).

### NOT closed by r16 cycle (the central question, restated)

- **[R10-T1 → R16-T1]** `restore_sandbox` end-to-end rollback via `fail_submit` / `fail_livez` — **8th-round EMERGENCY.** No fixture wires either flag.
- **[R10-T2 → R16-T1]** `persist=Some(_)` chain — **8th-round EMERGENCY.** All 5 pg-e2e `restore_sandbox` callsites pass `None`.
- **[R15-T1a]** future-drop coverage of `reserve_vm_index_with_retry` — **2nd-round carry.** No test wraps in `compio::time::timeout`.
- **[R15-T1b]** OS-thread detach pattern at `admin_handlers.rs:1339-1392` — **6th-round byte-coverage-zero.** R15-Q1 refactor at `7469118e` touched the surface without adding a test.
- **[R15-T2 → R16-T2]** constant-arithmetic-only test pattern — **3rd-round carry under new name.** C-8b added one more example.

## Carry-forward (still open from earlier rounds)

- **[R9-T6 / R11-T4 / R12-T4 / R13-R15 carry]** `derive_agent_url` parity — STILL three hardcoded `7777` sites at `restore_handler.rs:1144` (StubRestoreBackend variant `17777u16+vm_index`), `:1662` (`RealRestoreBackend::derive_agent_url`), `:1741` (`register_restored`). Canonical site `nomad_ch.rs:792 / :1532` uses `{AGENT_PORT}` constant. **9th-round carry.**
- **[R10-T3]** `read_sandbox_id_from_sources` cross-source (env-empty + file-present) test in `crates/sandbox-agent/src/handlers.rs`: 7 tests, none with both env-AND-file. **7th-round carry.**
- **[R10-T4]** `RootKek::from_env` production entry under uid invariant: still only `from_path`-driven tests. **7th-round carry.**
- **[R10-T5]** `TieredSnapshotStore::put` L2-detach behavioural test: r13's C-3 test at `snapshot_store_gcs.rs:1405-1439` partially exercises spawn-side; fire-and-forget joinability + L2-failure / L1-success-divergence still unpinned. **7th-round carry.**
- **[R9-T3]** `do_restore_inner` spawn_blocking panic-recovery — STILL 8 sites with JoinHandle discarded via `let _ = ... .await`. **8th-round carry.**
- **[R9-T5]** `build_restore_nomad_job_json` integration via `submit_restore_job` STILL untested. r12-I1's +5 builder-shape tests don't drive env→mode→jobspec→POST. **7th-round carry.**
- **[R9-T8]** Sweep takeover end-to-end for `Restoring` / `RestoringCold` transient states — only `Snapshotting` exercised at pg-e2e. **8th-round carry.**
- **[R9-T9]** `/_clock_resync` 4 KiB body-cap 413 test — still no analog of `proxy_http_body_cap_413`. **8th-round carry.**
- **[R9-T10]** `clock_resync_post_restore` controller-side slow-agent + 500-from-agent + malformed-body tests: still 4 tests only. **8th-round carry.**
- **[r4-T2 / r5-P1b / r7-P1 / wake-path latency canary]** still no compio-tick canary in `do_snapshot_inner` or `do_restore_inner`. **12th-round carry.**
- **[r3-T3 / wrapper script behavioural coverage]** still ZERO behavioural test for `crates/sandbox/scripts/nomad-vm-wrapper.sh`. **13th-round carry.**
- **[T8 ControllerIdleSnapshotter non-pg coverage]** still byte-coverage-zero outside cluster smoke at `crates/sandbox/src/sweep.rs:314-407`. **7th-round carry.**
- **[r6 MEDIUM script artifact consistency tests]** still unwritten. **10th-round carry.**
- **[r8 LOW sig.rs:1475 1-day-future skew-bypass]** still couples nonce-replay assertion to `ts_now() + 86_400`. **8th-round carry.**
- **[R9-P2 (deferred) A2b `verify_metadata_only` zero callers]** still untested at `snapshot_store_gcs.rs`. **8th-round carry.**
- **[R11-T5 / R13-T3 (re-filed)]** `read_root_owned_secret_file` extract still blocked on R13-Q2 error-envelope harmonisation. **5th-round carry.**
- **[Integration test dir stasis]** `crates/sandbox/tests/` + `crates/sandbox-agent/tests/` — **zero new tests for 7 consecutive cycles.** Verified at HEAD `792a7aa5`: `git log --oneline b8fae7b7..792a7aa5 -- crates/sandbox/tests crates/sandbox-agent/tests` returns empty.

## Cross-lens consensus

- **architecture-r16 r16-A1** ↔ **test-cov-r16 R16-T2**: SAME defect class — C-8b's `2 × fence` is a numeric coincidence, not a model; the `c8b_*` test at `:1649-1684` pins the coincidence at one input point. Architecture says "model the components"; test-cov says "property-test the (fence, nomad_purge, deadline) tuple." Same recommendation, two lenses, two rounds running.
- **concurrency-r16** ↔ **test-cov-r16 R16-T1**: per brief, concurrency-r16 still has no test for the detach pattern at `admin_handlers.rs:1339-1392`. Both lenses agree on the R15-T1b ask. Three lenses (test-cov-r15 + r16, concurrency-r15 + r16) have asked.
- **smoke-r11 cluster lens** ↔ **test-cov-r16 R16-T1**: smoke-r11 verdict: *"no remaining knob inside the synchronous-response contract."* Test-cov verdict: *"the integration-coverage gap won't close pre-C-7-LT regardless of how many constants we pin."* Both lenses agree the structural fix (C-7-LT-PR1+PR2) is the only path; both lenses also agree the four R15-T1 sub-asks remain VALID coverage independent of C-7-LT (they protect the rollback path that survives the migration).

## Lens hand-off

**To architecture-r17**: R16-T2's property-test rewrite of the 5-test `r14a6_*` + `c8b_*` family directly receipts r16-A1's "model not coincidence" recommendation. The property test would compile-fail when the Nomad purge tail timeout (`nomad_ch.rs:1051`'s 30 s) changes without `restore_handler.rs`'s formula updating — that's the architectural property "decompose-then-bound" applied to the test.

**To concurrency-r17**: the R15-T1b OS-thread detach extraction (R16-T1 sub-#4) is the bridge to the concurrency lens's recommended `dual-task compio scenario` testing infrastructure. Both lenses converge on the same extract-to-helper-then-test pattern; one cycle to land closes the carry for both lenses simultaneously.

**To test-cov-r17**: if C-7-LT-PR1 lands the wake_jobs CRUD + 4-6 pg-gated tests, audit specifically whether (a) the pg fixtures inject `StubRestoreBackend` failures (closes R10-T1), (b) the state-machine PR2 tests cover the rollback CAS transitions (closes R9-T8 indirectly), (c) any test surfaces `persist=Some(_)` (closes R10-T2). If 0/3, R17 stays at EMERGENCY. If 2/3 or 3/3, R17 can de-escalate to CRITICAL with a closure path.

## Notes for the next round

**The central question this cycle**: PR1+PR2 in flight is the structural fix; what does the cycle delta look like? Answer: **+12 sandbox-lib tests this cycle (vs. r15's 0/0), but ZERO in the integration test directory for the 7th consecutive cycle. The cycle ADDED 6 best-in-class tests (A1-FOLLOWUP) and 1 more constant-arithmetic test (C-8b). The wedge surface (`restore_sandbox` end-to-end with `fail_submit`/`fail_livez`) is untouched, and PR1+PR2 will close it ONLY IF the pg-gated fixtures inject the stub failure flags.**

**R16-T1 hold rationale (EMERGENCY → EMERGENCY hold, no de-escalation)**:
1. r15 closed at EMERGENCY with two cycles of "cluster smoke is the validation gate" admissions in commit messages.
2. r16 adds smoke-r10 (C-8b NEW, confirmed at smoke-r11) and smoke-r11 (C-8c NEW). 10+ distinct bugs across 11 cycles. The 1-cycle-1-bug pattern holds.
3. The +12 unit tests this cycle, while a positive delta, do NOT touch the R10-T1 surface. The A1-FOLLOWUP tests are excellent but orthogonal to wake. The C-8b test is one more instance of R16-T2's defect class.
4. Smoke-r11 reviewer concluded "no remaining knob in the synchronous-response contract." Test-cov agrees: the four R15-T1 sub-asks survive the C-7-LT migration as rollback-path coverage; they remain valid pre-AND-post-C-7-LT.

**R17 priority directive**: pin PR1+PR2 to land WITH at least one fixture from R16-T5's 80-LOC sketch (the `wake_fails_on_submit_returns_503_and_reverts_row` pg-e2e). If PR1+PR2 ships without that fixture, R17 will be a HOLD-AT-EMERGENCY with the structural fix landed and the EMERGENCY carry still open — a configuration the audit has not yet seen and would represent a regression in cross-lens coordination.

**Cycle defining pattern (r16 update)**: r13 noted "firefighting beats systematic drain." r14 noted "firefighting now provably misses the fire." r15 noted "firefighting has stopped adding tests entirely; harness in active regression." r16 notes "firefighting added 12 tests this cycle, ALL on the wrong gaps — the wedge surface is unmoved while the policy formula and config validation surfaces are over-covered." The trajectory: r13–r14 added 1–4 tests per cycle (slowing); r15 added 0 (flatline); r16 added 12 (recovery) — but the integration gap is unchanged for the 7th consecutive cycle. **The team has demonstrated the capacity to add 12 unit tests in a cycle; it has not demonstrated the capacity to add ONE integration test in the entire 7-cycle stretch since r9.**
