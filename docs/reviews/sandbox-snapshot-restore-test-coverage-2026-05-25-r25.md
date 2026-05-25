# Sandbox/snapshot-restore — test-coverage r25 review

Date: 2026-05-25 (UTC). HEAD at audit: `92c45d26` (worktree tip
`c729c2b8` is the pin-bump for the stress-r3 cluster cycle).
Round 25. Prior: `docs/reviews/sandbox-snapshot-restore-test-
coverage-2026-05-25-r24.md` (HEAD `e6363fce`).

Bundle landed since r24 (controller v34 + driver v14): `61492e54`,
`dd2079a9`, `1fb69840`, `d638b10f`, `e82bffd7`, `6b240683`
(controller-side) and `177ff165` (driver-side, cross-worktree).
Pin-bumped at `c729c2b8` for the in-flight stress-r3 cycle.

## TL;DR

- **R24-T1 (stress harness un-versioned) — PARTIAL.** Byte-exact
  333-LOC import (`61492e54`) + SHA-pin install (`dd2079a9`)
  landed. The r24-asked "one-line invariant test under
  `crates/sandbox/tests/`" did NOT land. No `*.rs` file references
  `snapshot_stress.py` — verified via `Grep "snapshot_stress|
  stress_harness|stress\.py" crates/sandbox/tests` → 0 matches.
  Downgrade from CRITICAL to IMPORTANT: reviewability /
  diffability closed; silent-regression risk persists.
- **R24-T2 (cold-boot `workspace.img` mirror test) — STILL OPEN.**
  Bundle added 5 tests but all 5 are for the pure
  `extract_failed_task_event_msgs` helper. The cold-boot side of
  the staging precondition is asserted only at the helper level
  (`assert_disk_image_present_*`, `create_ext4_image_if_missing_
  skips_when_file_exists` / `..._skip_path_rejects_zero_byte_file`).
  No `create()`-level test asserts "no Nomad RPC fires when
  `workspace.img` is absent on disk". R24-T2 carries → R25-T2.
- **R24-T3 (tap-leak coverage) — DRIVER-SIDE PARTIAL, CONTROLLER
  STILL OPEN.** Driver v14 added 3 in-process Go tests at
  `nomad-driver-ch/tests/stop_task_test.go:464,:516,:568` for
  `DestroyTask` defensive cleanup. Counter `tapsOrphanedTotal` is
  exercised. **No multi-cycle test** (`StartTask → DestroyTask →
  StartTask(same vmIndex)`) — the exact race that produced
  stress-r2's 9 stranded interfaces. Controller-side
  `sandbox_stranded_tap_total` / `/sys/class/net` sweep ask from
  r24 still absent. → R25-T3.
- **NEW [R25-T4] zero unit-test coverage for the host_dir GC
  sweeper.** `e82bffd7` shipped 362 LOC of
  `sweep::spawn_host_dir_gc` / `run_host_dir_gc_once` with the
  commit-message-stated rationale "the controller-side e2e is
  already comprehensive; the cluster stress run is the real
  test." Six eligibility gates with destructive-on-misfire
  semantics — `users/` skip, non-UUID skip, mtime grace,
  terminal-state, pending-wake-jobs, shutdown-mid-sweep — all
  un-asserted. Verified via `Grep "run_host_dir_gc|host_dir_gc|
  HOST_DIR_GC" crates/sandbox` returning only the impl + doc-
  comment hits.
- **NEW [R25-T5] no end-to-end test for verbatim-driver-msg
  propagation.** Bundle's 5 pure-helper tests pin shape +
  truncation but the chain `extract_failed_task_event_msgs(alloc)`
  → `wait_for_alloc_running` Err → `RestoreHandlerError::Backend`
  → `sanitize_error_message` → `wake_jobs.error_message` →
  `GET /wake/{id}` 200 `message` has zero tests at any hop after
  the entry point. Verified via `Grep "extract_failed_task|
  driver_msgs|workspace\.img does not exist|DisplayMessage|
  verbatim" crates/sandbox/tests/sandbox_pg_e2e.rs` → 0 matches.
- **Lib delta r24→r25: +21** (433 → 454, verified
  `cargo test -p zeroship-sandbox --lib` = 454 PASS / 0 fail /
  1 ignored at HEAD `92c45d26`). Decomposition: ~16 from T1
  admin-RO bundle (`97fcbcda`+`7b5d84f5`+`038ff3c7`) + 5 from
  v34 verbatim-msg helper (`d638b10f`). Pg-gated unchanged at 91.
  **Of the +21 lib delta, zero assert any of the four v34
  contracts at their boundaries** (cold-boot preflight, sweeper
  gates, end-to-end propagation, CreateGuard leak-on-drop).
- **stress-r3 (currently running) carries deterministic-test
  burden the in-tree harness can't.** Four v34/v14 contracts
  reach their first non-trivial input at the cluster — cluster
  is a stochastic oracle, not analytic. See "What stress-r3 will
  exercise that unit tests miss" below.

## CRITICAL

None. R24-T1's reviewability emergency closed; the silent-
regression residue is IMPORTANT.

## IMPORTANT

### [R25-T1] R24-T1 closure incomplete: harness in-repo but polling contract un-pinned

- **Where**: `crates/sandbox/scripts/snapshot_stress.py` (380
  LOC, +47 docstring extension). Install pin at
  `crates/sandbox/scripts/gcp-worker-startup.sh:217`
  (`SNAPSHOT_STRESS_SHA256="89ba229e…"`, FATAL on mismatch at
  `:238-244`). No `crates/sandbox/tests/*.rs` references the
  script.
- **The reviewability emergency closed**; the silent-regression
  axis didn't. The SHA-pin guarantees what the *worker* installs
  matches a known SHA. Nothing guarantees the in-repo file's
  polling-loop contract matches its intent. A future edit that
  flips 200 happy-path detection to 202-greedy would change wake-
  success counts without any CI signal. The pin bumps in
  lockstep with the SHA — drift is undetected, not prevented.
- **What WOULD close it**: a `crates/sandbox/tests/stress_harness_
  invariant.rs` that either shells out against a mock HTTP server
  speaking the two-step polling contract (asserting 200 ok / 200
  failed / 202→poll branches), or parses the regex/literal
  patterns out of the python file as test data. ~80-120 LOC.
- **Why dd2079a9's post-commit caveat makes this MORE urgent**:
  the SHA pin (`89ba229e…`) tracks the GCS object, not the in-
  repo file (which is `c4d2a2d4…` after the docstring extension).
  At the next stress window the pin AND in-repo SHA move
  together — but neither anchors a behaviour test. A polling-
  loop change landing in the same commit would never trip CI.
- **Severity IMPORTANT**: not a stress-r3 blocker (the current
  SHA is known-good against the stress-r1 polling rewrite);
  blocks defensibility of the next harness edit.

### [R25-T2] No cold-boot `create()`-level test for `workspace.img` staging precondition

- **Where**:
  - Production: `nomad_ch.rs:719` (mkdir + `guard.host_dir_created
    = true`), `:723-:725` (`create_ext4_image_if_missing` calls),
    `:3656-:3716` (helper), `:3740-` (`assert_disk_image_present`).
  - Helper tests (5): `nomad_ch.rs:5049, :5089, :5124, :5146,
    :5175`. Cover idempotency + post-condition on skip + the
    three asserter invariants.
  - Restore-side mirror (gold standard): `restore_handler.rs:3094`
    (`submit_restore_job_rejects_missing_workspace_img`), `:3130`
    (`..._user_home_img`).
- **What's missing**: a `nomad_ch::tests::create_must_stage_
  workspace_img_before_submit_nomad` test that drives `backend.
  create(sid, "usr_test", "proj_test")` with `workspace.img` NOT
  staged (stub `create_ext4_image_if_missing`), asserts the error
  message names `workspace.img`, AND asserts the fake Nomad addr
  was never hit (no `submit_nomad_job` call). The `spawn_404_mock`
  fixture exists at `nomad_ch.rs:~6300`. ~80 LOC mirror.
- **Why R22-T1 + the 5 helper tests don't substitute**:
  - R22-T1 asserts emitters AGREE on JSON field names; says
    nothing about controller staging precondition.
  - The 5 `extract_failed_task_event_msgs` tests are pure-
    function tests on hand-crafted JSON. Never call `create()`.
- **Why this still matters under v34**: the sweeper-owned-cleanup
  invariant depends on the cold-boot side ALWAYS staging
  `workspace.img` before submit (otherwise the leaked dir is
  empty and the driver preflight reproduces Bug 1 even with the
  sweeper). Today the only enforcement is source-order in
  `create()`. A future refactor reordering staging after
  `submit_nomad_job` breaks zero unit tests; only stress-rN
  catches it. R24-T2 carries verbatim, 3rd cycle.
- **Severity IMPORTANT**.

### [R25-T3] Driver defensive-tap tests are single-pass — no multi-cycle race shape

- **Where** (cross-worktree, `/home/ruiyang/Projects/appbase/
  .worktrees/nomad-driver-ch/nomad-driver-ch/`):
  - `tests/stop_task_test.go:464` — partial-init handle
    (`h.tap == ""` → defensive deletes `zsbx-nm-7`, counter +1).
  - `:516` — happy-path (`h.tap == zsbx-nm-7` → no double-
    delete, counter unchanged).
  - `:568` — operator-NetSpec divergence (both fire in order).
  - Production: `ch/stop_task.go:283-:378` (new defensive pass),
    `ch/metrics.go` (`tapsOrphanedTotal` atomic counter),
    `ch/export_test_api.go:274-:292` (`SetHandleTapForTest`).
- **What's well-covered**: the hook's single-pass invocations,
  the `removeTapFn` seam, the counter's pre/post deltas, the
  `!=`-guard logic.
- **What's MISSING**:
  - **Multi-cycle race**: stress-r2's 9 stranded interfaces came
    from `VmIndexAllocator` reusing indices 4-12 across cycles
    while a peer alloc's `DestroyTask` didn't fire. No test
    fires `StartTask → DestroyTask → StartTask(same vmIndex)`
    and asserts the second `StartTask::setupTapForVM` succeeds.
  - **Concurrency / `go test -race`**: all three tests serialise
    `closeRunner(); StopTask; DestroyTask`. No goroutine fan-out,
    no destroy-vs-start race oracle.
- **Sandbox-side**: still NO controller-side
  `sandbox_stranded_tap_total` + `/sys/class/net` sweep extension
  in `cleanup_orphans_at_startup`. R24-T3's observability ask
  unchanged.
- **Severity IMPORTANT**. The defensive hook itself is
  exercised; the race-detection shape is what's untested. The
  cluster oracle is doing this work.

### [R25-T4] [NEW] Zero unit-test coverage for the host_dir GC sweeper

- **Where**:
  - Production: `crates/sandbox/src/sweep.rs:909-:1131` (~220
    LOC). Wired via `lib.rs:1036` (`spawn_host_dir_gc`).
  - Test files: **NONE**. The commit message at `e82bffd7`
    explicitly defers: *"A pg-gated test for the eligibility-
    gate matrix can land in `tests/sandbox_pg_e2e.rs` (deferred
    — the controller-side e2e is already comprehensive; the
    cluster stress run is the real test)."*
- **Six gates, all un-asserted, three with destructive-on-
  misfire semantics**:
  1. **`users/` literal skip** (`sweep.rs:973`). A regression
     reaping `<host_state_dir>/users/` wipes every user's
     home.img tree on one sweep tick.
  2. **Non-UUID-name skip** (`sweep.rs:981`). A regression
     bypassing `Uuid::parse_str` is destructive to any
     operator artefact under a custom-named subdir.
  3. **mtime grace** (`sweep.rs:1015`, default 1 hour, floor
     60s). A regression confusing `mtime` vs `ctime`, or
     misapplying the grace, reproduces stress-r2 Bug 1 with
     the sweeper as the new offender.
  4. **Terminal-state gate** (`sweep.rs:1050-:1056`). The match
     pins exactly `{Stopped, Lost, Orphan}`. A regression
     adding `Snapshotted` silently breaks wake by reaping
     `workspace.img` from a sandbox waiting to wake.
  5. **Pending-wake-jobs gate** (`sweep.rs:1077-:1102`). The
     `find_pending_wake_for_sandbox` filter is the only thing
     stopping a mid-wake reap.
  6. **Shutdown observability** (`sweep.rs:949`). Long-running
     `rm -rf` mid-tick must observe `state.shutdown_requested()`
     between entries.
- **What WOULD close it** (representative):
  - Pg-gated test driving `run_host_dir_gc_once` against a
    fresh tempdir host_state_dir with fixture combinations:
    fresh dir under grace (skip), stale + `Stopped` row (reap),
    stale + `Snapshotted` row (preserve), stale + `Stopped` +
    pending wake_job (preserve), `users/` subdir (preserve).
    `(scanned, reaped)` return value pins the assertion. ~120
    LOC.
  - Lib-test for the non-DB gates (`users/` skip, non-UUID
    skip, mtime grace) using a stub DB. ~50 LOC.
- **CreateGuard's drop-leak path also untested**. The three
  `CreateGuard` tests (`nomad_ch.rs:4010, :4060, :4122`) all set
  `host_dir_created = false` to skip the rm -rf. None set
  `host_dir_created = true` and assert the v34 invariant
  ("leak the dir with INFO log, do not remove"). The leak-and-
  defer-to-sweeper contract is asserted ONCE in the codebase: at
  `nomad_ch.rs:6372` (`stop_for_real_leaks_host_dir_for_sweeper`,
  exercising `stop_inner` step 5 — not `CreateGuard::drop` step
  3). `Grep "host_dir_created = true|host_dir_created: true"
  crates/sandbox/src/backend/nomad_ch.rs` returns only the
  production-code line at `:719`.
- **Why "the cluster stress run is the real test" is the wrong
  framing here**: three gates have destructive-on-misfire
  semantics. The cluster catches positive failures (wakes
  fail → workspace.img reaped) but only AFTER landing in
  production shape. A unit test catches the same bug before
  any cluster cycle costs $20-30 of compute. This is the
  textbook unit-vs-cluster economics inversion.
- **Severity IMPORTANT**. The sweeper is the v34 contract per
  the commit message ("THIS is the load-bearing fix"). Six
  gates, three destructive-on-misfire, zero unit coverage. Test-
  cov-CRITICAL was avoided this round only because R24-T1's
  reviewability emergency closed.

### [R25-T5] [NEW] No end-to-end test that verbatim driver text reaches `wake_jobs.error_message` / wire `message` field

- **Where**:
  - Pure helper: `nomad_ch.rs:2756-:2799` (production),
    `:4235-:4358` (5 unit tests).
  - Cold-boot caller: `nomad_ch.rs:2645` (`let driver_msgs =
    extract_failed_task_event_msgs(a)` composed into the
    `wait_for_alloc_running` Err).
  - Restore caller: `restore_handler.rs:2577-:2588` (same
    composition shape).
  - Sink: `wake_machine.rs` (writes `error_message` to
    `wake_jobs`); `GET /admin/sandboxes/{id}/wake/{wake_id}`
    response `message` field per §10.0.
- **5 helper tests cover**: shape (verbatim text preservation,
  task-name prefix), no-failed-task / missing-TaskStates defence,
  empty-DisplayMessage defence, 2-KiB cap with `…(truncated)`
  sentinel. **Pure-function level only.**
- **Missing seams**:
  - **`wait_for_alloc_running` end-to-end**: no test fires a
    fake-Nomad alloc with `Failed: true` + DisplayMessage and
    asserts the returned `Err(String)` carries the verbatim
    text. The existing `wait_for_alloc_running_surfaces_
    unreachability` at `nomad_ch.rs:4371` covers only the no-
    Nomad-reachability path.
  - **Pg-gated**: no test drives a WakeMachine through a
    `restore_failed` terminal where the error string contains
    the verbatim driver msg and asserts `wake_jobs.error_message`
    carries it post-write. The R23-I1 terminal-overwrite tests
    at `sandbox_pg_e2e.rs:5530, :5617` cover counter bumps with
    placeholder error strings.
  - **Admin handler** (`sandbox_admin_e2e.rs`): no test hits
    `GET /admin/sandboxes/{id}/wake/{wake_id}` and asserts the
    response `message` field includes the verbatim driver text
    (subject to RO-bearer sanitisation per security-r25).
- **Why this matters NOW**: pin-bump commit `c729c2b8` calls
  out the dual fix: "Targets the stress-r2 host_dir leak +
  opaque error envelope." The "opaque error envelope" half
  depends entirely on the chain working end-to-end. 5 tests at
  the entry, 0 at the exit. A regression in any intermediate
  hop (`sanitize_error_message` over-redacting, `Backend(e).
  to_string()` losing inner text, wake_machine truncating,
  admin handler field-masking) silently re-introduces the
  opaque envelope the v34 fix exists to remove.
- **Cross-lens**: security-r25 R25-S1 flags the SAME chain but
  for path-leak risk. Two seams, both lenses; one PR can land
  both.
- **Severity IMPORTANT**. Two test seams (pg-gated + admin-
  handler), ~100 LOC total.

## MINOR

### [R25-T6] [NEW] `extract_failed_task_event_msgs` truncation lacks UTF-8 boundary safety + test

- **Where**: `nomad_ch.rs:2787` (production):
  `&trimmed[..PER_TASK_CAP]`. Test at `:4332-:4358` uses
  `"X".repeat(10_000)` — ASCII.
- **What's missing**:
  - Assertion that the sentinel is at the END (not mid-string
    from a buggy slice).
  - Assertion that the truncated body preserves the FIRST 2048
    bytes verbatim.
  - UTF-8 boundary safety: `&trimmed[..2048]` will `panic!` if
    byte 2048 is mid-UTF-8 codepoint. A driver emitting
    multi-byte error text (operator locale, emoji, etc.) at
    the cap edge crashes the controller. Fix: `floor_char_
    boundary(2048)` (1-line code change + 1 regression test).
- **Severity MINOR**: the helper's caller chain shields most
  drivers, but theoretical panic on a real input shape exists.
  Low-incidence, easy fix.

### [R25-T7] Trend delta convention — +21 lib (r24→r25) vs +5 (v34-bundle-only)

- The brief stated "Bundle added 5 new tests
  (extract_failed_task_event_msgs × 5). Tests 449 → 454." That's
  the v34-bundle-only delta against a post-T1 baseline of 449.
- r24→r25 lib delta against r24 HEAD `e6363fce`: **+21** (433 →
  454). T1 admin-RO bundle landed `7b5d84f5` + `97fcbcda` +
  `038ff3c7` (~16 lib tests for `with_admin_ro_token` +
  `assert_distinct_admin_tokens`); v34 bundle added the 5 helper
  tests.
- Both interpretations are defensible. The trend table below
  uses the r24-HEAD → r25-HEAD convention.

### [R25-T8] Working-tree compile error against HEAD is from an uncommitted R25-S1 fixer

- **Where**: `crates/sandbox/src/db.rs` (uncommitted) adds
  `WakeErrorCode::StagingPathMissing` at `:1521-:1551`. The
  `as_str` match at `:1621` is non-exhaustive after the add →
  `cargo test -p zeroship-sandbox --tests` fails.
- **Not from the v34 bundle.** At committed HEAD `92c45d26`,
  `cargo test --lib` = 454 PASS / 0 fail / 1 ignored. The mid-
  flight working-tree fixer is the R23-API1 / R25-S1 lens hand-
  off; when it lands, the exhaustive-match discipline at
  `db.rs:1588-1607, :1621-:1641` enforces the 3-site coverage
  add. No test-cov action beyond confirming the round-trip test
  (`wake_error_code_roundtrip_all_variants_known` at `db.rs:3577-
  3591`) gets the new variant.

## Trend table — carry from r24 with closure

| Cycle | sandbox lib | pg-gated | Δ lib | Δ pg | Notes |
|-------|-------------|----------|-------|------|-------|
| r17   | 373         | 74       | +29   | +9   | PR1+PR2 |
| r18   | 402         | 83       | +29   | +9   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r19   | 424         | 87       | +22   | +4   | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| r20   | 426         | 87       | +2    | 0    | R19-I4 shims + R19-T1 widening |
| r21   | 428         | 87       | +2    | 0    | r17-Q3 DataIntegrity round-trip |
| r22   | 432         | 89       | +4    | +2   | R10-API4 + R20-C1 + R22-I1 counter |
| r23   | 432         | 89       | 0     | 0    | (no audit; smoke-r23 cycle) |
| r24   | 433         | 91       | +1    | +2   | R22-T1 parity + R23-I1 counter e2e |
| **r25** | **454**   | **91**   | **+21** | **0** | T1 admin-RO bundle (~16) + v34 helper (5); pg unchanged |
| r9→r25 | +158       | +17      | —     | —    | wedge + sweep + sanitizer + probe + retry + drift + envelope + parity + counter-e2e + RO-admin + driver-msg helper |

Of the +21 lib delta, ZERO assert any of the four new v34
contracts at the boundary they cross (cold-boot create
preflight, sweeper eligibility matrix, end-to-end driver-msg
propagation, CreateGuard leak-on-drop).

## Carry-forward table (r24 → r25)

| Tag | r24 status | r25 status | Note |
|-----|------------|------------|------|
| R24-T1 stress harness un-versioned | CRITICAL, OPEN | **PARTIAL → R25-T1** | Byte-exact import + SHA-pin landed; invariant test did NOT. Downgrade-on-progress to IMPORTANT. |
| R24-T2 cold-boot workspace.img preflight | IMPORTANT, OPEN | **STILL OPEN → R25-T2** | v34 bundle touched `create()` extensively; did NOT mirror restore-side preflight test pair. 3rd cycle. |
| R24-T3 tap-leak observability | IMPORTANT, OPEN | **DRIVER PARTIAL → R25-T3** | Driver v14 added 3 single-pass tests; no multi-cycle race test; sandbox counter absent. |
| R23-I1 counter sites + e2e | CLOSED | n/a | Re-validated by code-quality-r25 at wake_machine.rs:199-200. |
| R22-T3 retry-race pg test | IMPORTANT (escalated), OPEN (4th) | **OPEN, 5th** | No `insert_wake_job_retries_*` test landed. Concurrency-r24 silent. |
| R21-T1 r17-Q3 DataIntegrity integration | MINOR, OPEN (5th) | **OPEN, 6th** | Carry. |
| R21-T2 host-fence constants | DELIST | n/a | Stays delisted. |
| R18-T5 persist=Some chain | DELIST | n/a | Stays delisted. |
| R18-T7 `probe_and_classify` sibling loops | OPEN, carry | OPEN, carry | Still 0 hits. |
| R18-T9 OS-thread detach pattern | OPEN, carry | OPEN, carry | 4 sites uncovered. |
| R19-T2 takeover_once lib coverage | OPEN, carry | OPEN, carry | R19-C1 counter wired; loop itself untested. |
| R19-T3 R19-C1 claim-count metric | OPEN, carry | OPEN, carry | Co-file with R22-T3. |

## Bundle-specific would-have-caught analysis

### Does the bundle close R24-T3 (tap leak coverage)?

**Driver-side PARTIAL.** 3 new Go tests cover the defensive-
cleanup hook's single-pass invocations + counter dynamics. They
MISS the multi-cycle race (`StartTask → DestroyTask →
StartTask(same vmIndex)`) and don't run with `go test -race`.
The stress-r2 9-stranded-tap residue came from precisely the
multi-cycle reuse shape; v14's defensive cleanup should close
it inside `DestroyTask` but no test enforces the closure.

**Sandbox-side NO.** Controller-side counter / `/sys/class/net`
sweep extension in `cleanup_orphans_at_startup` unchanged.
Verified: `Grep "zsbx-nm" crates/sandbox/src` returns only the
doc-comment hits at `nomad_ch.rs:62,91,1923` and the wrapper
script's `ip link set $TAP up` lines — no controller code
inspects tap names.

### Does the bundle close R24-T2 (cold-boot mirror test)?

**NO.** The 5 new tests are all for the verbatim-driver-msg
helper, not for cold-boot preflight. The cold-boot side of the
staging precondition is asserted ONLY at the indirect helper
level:
- `create_ext4_image_if_missing_skip_path_rejects_zero_byte_file`
  (idempotent skip re-asserts the post-condition).
- `create_ext4_image_if_missing_skips_when_file_exists` (the
  idempotency contract).
- 3 × `assert_disk_image_present_*` (helper's invariants).

The `create()`-level contract — "if `workspace.img` is absent,
`submit_nomad_job` MUST NOT fire" — has no test. R24-T2 carries.

### Sweeper test coverage

**Zero.** See R25-T4 — six gates, three with destructive-on-
misfire semantics, all un-asserted. The commit message at
`e82bffd7` explicitly defers the eligibility-gate matrix to
pg-gated and notes "the cluster stress run is the real test."
The lib-test delta of +5 from this bundle is entirely on the
pure-helper side.

### Verbatim driver-msg propagation tests

**Pure helper only.** 5 tests at `nomad_ch.rs:4235-:4358` pin
shape + truncation. No test at any later seam:
`wait_for_alloc_running` end-to-end, `wake_jobs.error_message`
pg write, `GET /wake/{id}` 200 `message`. See R25-T5.

### R24-T1 closure verification

- Source under version control: YES (`61492e54`, 333 LOC +
  docstring extension).
- Install SHA-pin: YES (`dd2079a9`, `gcp-worker-startup.sh:217`).
- One-line invariant test: NO.

The R24-T1 closure entry in `deferred.md` (per `1fb69840`)
documents both commits and the post-commit caveat (GCS mirror
stale until re-upload). Closed for reviewability + diffability;
silent-regression axis remains. R25-T1 picks this up.

## What stress-r3 (currently running) will exercise that unit tests miss

Enumerated against the four v34/v14 contracts:

1. **Cleanup-vs-retry race on cold-boot** (v34 sweeper-owned
   cleanup). 60 cycles × 3 workers concurrent. **No deterministic
   test** pins the "host_dir leaks per-alloc, sweeper-owned
   cleanup" invariant — R25-T4. Cluster is the only oracle.

2. **Multi-cycle vm_index reuse with defensive tap cleanup**
   (v14 driver). 60 cycles WILL reuse `vm_index` 4-12 multiple
   times per worker. **No driver test** fires this multi-cycle
   shape — R25-T3.

3. **Verbatim driver-msg propagation end-to-end** (v34
   controller). Stress-r3's failure cases (if any) WILL produce
   `wake_jobs.error_message` rows that should contain the
   `disk[N] /…/workspace.img does not exist` text (cold-boot) or
   restore-path equivalent. **No end-to-end test** — R25-T5.

4. **Sweeper not racing live wakes** (R25-T4 gate 5). 60-cycle
   stress includes SNAPSHOT→WAKE. The 5-min sweep cadence vs
   ~46s WAKE wall gives ~9% per-tick collision over the run.
   `find_pending_wake_for_sandbox` gate MUST suppress reaping.
   **No test** — R25-T4.

### Gaps cluster signal still has to close (independent of
stress-r3 verdict)

- **Deterministic pin on the v34 mental model.** If stress-r3
  is GREEN, the model is validated stochastically; if RED, the
  model has no unit-test anchor for re-examination. R25-T4 is
  the lens hand-off regardless of outcome.
- **UTF-8 truncation safety** (R25-T6). Cap-edge multi-byte
  panic theoretical, not stress-surfacing.

## Lib tests gating cutover

- **454 lib + 91 pg-gated + ~75 other integration**
  (`sandbox_admin_e2e.rs:26 ntex` + `sandbox_persist_e2e.rs:0` +
  `sandbox_preview_e2e.rs:6` + `sandbox_preview_share_e2e.rs:22`
  + `sandbox_preview_ws_e2e.rs:7 compio` + `sandbox_typed_id_
  e2e.rs:12` + `scripts_lint.rs:2`). Plus 130 driver-side (+3
  from v14).
- **Stress-r3 gate sufficiency**: existing 454 lib + 91 pg
  exercise the bundle's pure-helper additions and T1 admin-RO
  scaffolding. Four high-leverage seams remain un-pinned:
  cold-boot create preflight (R25-T2), sweeper eligibility
  matrix (R25-T4), end-to-end driver-msg propagation (R25-T5),
  multi-cycle vm_index race (R25-T3).
- **T-8b-cutover gate sufficiency**: INSUFFICIENT on R25-T4
  (destructive-on-misfire with zero unit coverage). Cutover
  should NOT proceed until at least the three high-consequence
  sweeper gates (`users/` skip, terminal-state gate, pending-
  wake-jobs gate) have unit-level tests. ~80 LOC lib using
  a stub DB; ~120 LOC pg-gated.

## Cross-lens consensus

- **R25-T1**: test-cov only. SHA-pin closes the security /
  reproducibility surface.
- **R25-T2**: 3-lens convergence. code-quality-r25 R25-I1
  (path-helper duplication), security-r25 R25-S1 (path-leak via
  error string), test-cov-r25 R25-T2 (missing test). One PR
  should land all three.
- **R25-T3**: cluster-lens (stress-r3) + test-cov-lens. Cross-
  worktree driver code; no other lens.
- **R25-T4**: test-cov-only. Sweeper contract is internal;
  destructive-on-misfire is a test-cov-priority pattern.
- **R25-T5**: test-cov + security convergence. test-cov asks
  for propagation invariant; security asks for sanitisation
  invariant. Both seams (pg-gated + admin-handler) carry both
  lenses.

## Lens hand-off

- **api-surface r25+**: R25-T2 closes a sibling of R22-T1 /
  R24-T2. Mirror-on-cold-boot test pair should land with the
  R25-I1 path-helper-extraction fix.
- **architecture r25+**: R25-T4 is the load-bearing fix's test
  debt. Three of six gates have destructive-on-misfire
  semantics; "the cluster stress run is the real test" is
  unsound for these. R25-T4 lib-test surface should land BEFORE
  the next driver-v15 / controller-v35 bump.
- **code-quality r25**: R25-T6 (UTF-8 boundary safety) is a
  1-line code fix + 1-test add.
- **concurrency r25**: R22-T3 (R20-T1 retry-race pg test) now
  5 rounds open. r24's escalation rule has expired without
  resolution. R25-T3 is concurrency-adjacent.
- **security r25**: R25-T5's chain shares the propagation
  surface with R25-S1. Paired test-cov + security-cov landing.

## To test-cov r26 backlog (~450 LOC total)

1. **R25-T4** sweeper eligibility-gate matrix tests — ~120 LOC
   pg-gated + ~50 LOC lib (non-DB gates). IMPORTANT.
2. **R25-T2** cold-boot `create_must_stage_workspace_img_before_
   submit_nomad_job` test — ~80 LOC mirroring
   `submit_restore_job_rejects_missing_workspace_img`. IMPORTANT.
3. **R25-T5** end-to-end driver-msg propagation: pg-gated
   (`wake_jobs.error_message` carries verbatim) + admin-handler
   (`GET /wake/{id}` 200 `message`). ~100 LOC. IMPORTANT.
4. **R25-T1** `snapshot_stress.py` polling-loop invariant test
   — ~120 LOC under `crates/sandbox/tests/`. IMPORTANT.
5. **R25-T3** (cross-worktree) multi-cycle vm_index race test
   in `nomad-driver-ch/tests/` firing `StartTask → DestroyTask
   → StartTask(same vmIndex)` — ~80 Go LOC. IMPORTANT.
6. **R22-T3** (carry, 5th cycle) — R20-T1 retry-race pg tests
   — ~80 LOC.
7. **R25-T6** UTF-8 cap safety + regression test — ~20 LOC.
   MINOR.

Delisted this round: none.

## Notes for r26

- **The big shift this round**: the v34 bundle landed extensive
  CODE changes with a TEST debt the commit messages explicitly
  acknowledge (`e82bffd7`'s "deferred"). The 454-lib-test count
  looks healthy but the seam-by-seam breakdown shows four high-
  leverage v34 contracts (cold-boot preflight, sweeper gates,
  verbatim-msg propagation, CreateGuard leak-on-drop) with zero
  deterministic guards. The cluster stress run is doing work
  unit tests should be doing — and the cluster costs $20-30/
  cycle vs ~$0 for unit tests.
- **Pattern carry-forward from r24**: r24's "the marginal gaps
  are in adjacent tooling, not in the crate itself" is no
  longer accurate. The v34 bundle moved gaps BACK INTO the
  crate (R25-T4 sweeper). r26+ should prioritise crate-side
  test debt over adjacent tooling.
- **stress-r3 outcome will tip priority order**: GREEN → R25-T4
  still IMPORTANT but no urgent backstop; RED → R25-T4 becomes
  the triage anchor, paired with whichever v34 contract the
  cluster signal implicates.
- **The harness-in-repo discipline is correct but incomplete**.
  R24-T1's CRITICAL was rationalised down because reviewability
  closed. Silent-regression needs a Rust-side invariant — see
  R25-T1. Without it the next harness edit lands SHA-pin-clean
  and behaviour-broken.
- **Highest-leverage closure for r26**: R25-T4 (sweeper gate
  matrix) — ~170 LOC, closes the v34 contract's destructive-
  semantics test debt, unblocks T-8b-cutover gate sufficiency.
  R25-T2 (cold-boot preflight) at ~80 LOC is second.
