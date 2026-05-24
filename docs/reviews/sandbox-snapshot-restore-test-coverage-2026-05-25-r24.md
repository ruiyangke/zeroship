# Sandbox/snapshot-restore — test-coverage r24 review

Date: 2026-05-25 (UTC). HEAD at audit: `e6363fce`. Round 24. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-25-r22.md`
(HEAD `8b366b6d`).

## TL;DR

- **The smoke→stress gap is the central finding.** Smoke-r23 ran exactly
  ONE cycle on a fresh host and passed (`082e6ddb`). Stress-r1 ran 60
  cycles across 3 workers and surfaced two bugs the smoke could never
  exercise: **C-N-W1** (cold-boot `workspace.img` staging contract gap,
  49/60 CREATE failures) and **C-N-W2** (`zsbx-nm-N` tap leak across
  vm_index reuse, 9 stranded interfaces on worker-1 alone). End-to-end
  rate **2/60 (3.3%)** — `docs/reviews/sandbox-snapshot-restore-
  cluster-2026-05-25-T8b-stress.md`.
- **R22-T1 parity test (`b6c55d93`) did NOT catch C-N-W1, and CANNOT
  catch C-N-W1 by construction.** It is a *JSON Config field-NAME*
  parity test (`restore_handler.rs:3741-3852`), not a *host filesystem
  staging* parity test. Cold-boot already emits all the right field
  names; the bug is that the driver's NEW pre-flight requires
  `workspace.img` *on disk before submit*, not that the field is
  missing from the JSON.
- **Coverage gap that WOULD have caught C-N-W1**: a controller-side
  unit test asserting "if `workspace.img` is absent on host_dir,
  `submit_nomad_job` MUST NOT fire" for the cold-boot path. Such a
  test does not exist at HEAD. The in-flight working-tree fix (see
  carry F1 below) adds it for the **restore** path only.
- **C-N-W2 (tap leak) has ZERO sandbox-side coverage** at any layer:
  unit, integration, or pg-gated. The driver lives in a different
  worktree, but the controller-side `cleanup_orphans_at_startup`
  (`nomad_ch.rs:431-464`) only purges JOBS — it never inspects or
  reclaims `zsbx-nm-<idx>` interfaces. A controller-side sweep that
  enumerates interfaces and counts strays would have flagged this.
  No such code exists; no such test exists.
- **R23-I1 closure verdict**: ADEQUATE. All three production call
  sites of `update_wake_job_state` in `wake_machine.rs` (`:130-156`,
  `:175-201`, `:518-540`) wire the counter. R23-I1's two pg-gated
  tests at `sandbox_pg_e2e.rs:5530, 5617` exercise the `:146` (Ok
  overwrite) and `:191` (Failed overwrite) bumps via end-to-end
  WakeMachine drive. The `:528` (`set_state`) bump is exercised
  transitively because the machine fires 5-6 intermediate `set_state`
  calls before terminal write; the `>= pre + 1` assertion captures
  every one. **No sibling sites missed.**
- **Stress harness is NOT versioned in-repo.** `snapshot_stress.py`
  lives at `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py`
  + `/tmp/t8b-stress/snapshot_stress.py.preasync.bak` on the operator
  machine only. `crates/sandbox/scripts/` carries zero `snapshot_stress`
  files (verified via `Glob`). The SHA `89ba229e…` is recorded in the
  T-8b-stress report but the **source is not under SCM** — a
  reproducibility gap that promotes the harness from "artifact" to
  "canonical contract" via GCS. This is **the standout new CRITICAL
  for r24**.

## CRITICAL

### [R24-T1] `snapshot_stress.py` harness is not version-controlled

- **Where**: `crates/sandbox/scripts/` — no `snapshot_stress.py`,
  no `stress/`, no equivalent under `tests/`. Confirmed via
  `Glob "**/snapshot_stress*"` returning zero hits in the worktree.
- **Symptom**: T-8b-stress (`docs/reviews/sandbox-snapshot-restore-
  cluster-2026-05-25-T8b-stress.md:27,156`) names two canonical
  locations: `gs://suger-dev-zsbx-artifacts/stress/snapshot_stress.py`
  (uploaded BEFORE provision; consumed by worker startup script's
  `gsutil cp` line) and `/tmp/t8b-stress/snapshot_stress.py.preasync.bak`
  (local backup of the pre-async version). Worker SHA pinned at boot
  to `89ba229e…`. **Both locations are off-repo.** Any future cluster
  cycle that needs to reproduce the stress numbers depends on a GCS
  blob that is not git-tracked, not signed, not reviewable, and not
  diff-able against historical versions.
- **Why CRITICAL**:
  1. The stress run is the **gate** between smoke-green and
     T-8b-cutover. If the harness has a bug (e.g., the pre-async
     version's `WAKE p50` masking that the polling rewrite caught),
     it changes the verdict — and the bug history is unreviewable.
  2. The "Option B" polling-aware rewrite is referenced as a smoke-r23
     P2 closure (`cluster-T8b-stress.md:27,166`), but no commit lands
     the script source; the closure is asserted against a GCS object
     hash, not against a tracked file. Pre-launch zeroship has no
     back-compat constraint (`AGENTS.md`), but **internal tooling
     reproducibility is its own discipline** and this leaks it.
  3. The "smoke missed C-N-W1 because the smoke only ran 1 cycle"
     analysis depends on the stress harness being trustworthy. If the
     harness itself silently regressed (e.g., a bash version masking
     the polling-window), nobody would notice — there's no CI run, no
     diff, no review against earlier behaviour.
- **Fix**: commit the polling-aware `snapshot_stress.py` under
  `crates/sandbox/scripts/snapshot_stress.py`. Update
  `gcp-worker-startup.sh` to `gsutil cp` the GCS copy OR vendor it
  directly from the repo (preferred — the worker startup script
  already has the workspace at boot via the cluster-provision pipe).
  Add a one-line invariant test under `crates/sandbox/tests/` that
  imports the script's `parse_terminal_response()` (or whatever the
  polling helper is called) and asserts the wake-poll loop's
  terminal-detection contract. Without that test the harness is just
  another untracked artifact.
- **Severity CRITICAL**: the harness is now part of the release
  gate; un-versioned tooling that gates production work is the
  textbook definition of test-cov-CRITICAL.

## IMPORTANT

### [R24-T2] No controller-side test for cold-boot `workspace.img` pre-submit invariant — C-N-W1's root coverage gap

- **Where**:
  - Production path: `nomad_ch.rs:690-701` (cold-boot stages via
    `create_ext4_image_if_missing`) → `:718-728` (`build_nomad_job_json`)
    → `:729-741` (`submit_nomad_job`). The "stage BEFORE submit"
    ordering is **only enforced by source order in `create()`** — no
    assertion, no test.
  - In-flight fix (working tree, uncommitted): `restore_handler.rs:
    2046-2089` adds `assert_disk_image_present(workspace.img)` +
    `assert_disk_image_present(home.img)` to `submit_restore_job`
    BEFORE `build_restore_nomad_job_json`. Mirror tests at
    `restore_handler.rs:3070-3145` (`submit_restore_job_rejects_
    missing_workspace_img` + `..._user_home_img`). **The mirror on
    the cold-boot side does not exist** — `nomad_ch.rs::create()` has
    no equivalent preflight call and no equivalent test.
- **Why this is the smoke-r23 → stress-r1 boundary**:
  Smoke-r23's single CREATE happened to win the `workspace.img`
  staging vs. driver-preflight race on a fresh host. Stress's 60
  CREATEs lost the race 49 times. A unit test of shape "cold-boot
  `create()` must call `assert_disk_image_present(workspace.img)`
  AFTER the truncate+mkfs+fsync BEFORE the `submit_nomad_job`" would
  have caught this **in-process without a cluster**. Specifically:
  - Mock `submit_nomad_job` via a feature flag or seam.
  - Bypass `create_ext4_image_if_missing` (or stub it as a no-op).
  - Call `create()` and assert that `submit_nomad_job` was either
    NOT called OR errored with a controller-side `workspace.img
    missing` message BEFORE the Nomad RPC.
  Roughly equivalent to the restore-path test pair the working tree
  just landed — but on the cold-boot site.
- **Why R22-T1 didn't catch it**: R22-T1 (`restore_handler.rs:3741-
  3852`) asserts JSON Config field-NAME parity between cold-boot's
  `build_nomad_job_json_with` and restore-path's
  `build_restore_nomad_job_json`. Both emitters emit the same field
  set; the bug is that the **on-disk state assumed by the driver's
  NEW preflight** (workspace.img present, non-zero, regular file)
  is not asserted at the controller side. R22-T1's design space is
  "do the two emitters agree on field NAMES?" — not "do the two
  call sites stage the same files?". **Different invariant.**
- **Fix**: ~60 LOC unit test in `nomad_ch.rs::tests` mirroring the
  shape of the working-tree's
  `submit_restore_job_rejects_missing_workspace_img` (`restore_handler.
  rs:3070`). Best done as part of the in-flight C-N-W1 fix PR.
- **Severity IMPORTANT**: closes the smoke→stress gap for the cold-
  boot half of the contract. Without it, the same class of bug
  (NEW driver preflight contract added; controller doesn't satisfy
  it) recurs on the next driver version bump.

### [R24-T3] Zero sandbox-side coverage for C-N-W2 tap-name reuse / leak

- **Where**:
  - Driver-side fix exists at `nomad-driver-ch` worktree, commit
    `3d03cb90` (`nomad-driver-ch/net: pre-delete on tap-add collision
    (T-8b-stress Bug 2)`). Out of audit scope.
  - Sandbox-side: `nomad_ch.rs:431-464` `cleanup_orphans_at_startup`
    purges JOBS via Nomad HTTP API. **It does not enumerate or
    reclaim `zsbx-nm-<idx>` interfaces.** No other code path in the
    sandbox crate inspects tap names. Verified via `Grep "zsbx-nm"`
    → only doc-comment hits at `nomad_ch.rs:62,91,1923` (descriptive,
    not load-bearing) and the wrapper script (`nomad-vm-wrapper.sh:
    250,727` — `ip link set $TAP up`, never `del`).
- **Symptom**: after 60 sequential cycles on one worker, 9 `zsbx-nm-4`
  through `zsbx-nm-12` interfaces survived in DOWN/NO-CARRIER state
  (T-8b-stress report `:92-93,121`). The vm_index allocator
  (`VmIndexAllocator` at `nomad_ch.rs:269`) reuses indices 4-12 within
  a few cycles; every reuse hit the driver's `Tap already exists`
  WARN → Exit-1 path → 9 of the 11 wakes that survived CREATE then
  died on the restore alloc.
- **What test boundary should have caught this**:
  - **Controller-side integration test** (no cluster, no driver):
    drive `create() → stop() → create()` with the same vm_index N
    times, mock the driver such that it fails when a "stale tap"
    sentinel file exists at `/tmp/zsbx-nm-<idx>.exists`. Assert that
    after cycle K, the controller either (a) deleted the sentinel
    pre-create, or (b) errored before submit with a structured
    `error_code=stale_tap_present`. Currently neither — the
    controller has no concept of "tap state on host" because the
    bash wrapper used to own that lifecycle and the Go driver was
    expected to replicate it without contract.
  - **`cleanup_orphans_at_startup` extension test**: assert it
    enumerates `/sys/class/net/zsbx-nm-*` and either logs the count
    or bumps a `sandbox_stranded_tap_total` counter. Currently
    impossible because the code path doesn't exist.
- **Why IMPORTANT not CRITICAL**: the driver-side fix at
  `3d03cb90` (pre-delete on tap-add collision) is the right layer;
  the controller doesn't need to own the tap. But the **defence-in-
  depth observability** (a counter that says "N stale taps survive
  on this host at startup") is a sandbox-side responsibility and is
  absent. T-8b-stress action item P3 (`cluster-T8b-stress.md:165`)
  proposes exactly this. **Test-cov claim**: it should land with a
  unit test that confirms the counter fires on a mocked
  `/sys/class/net` directory with a stale `zsbx-nm-9` entry.
- **Fix**: ~80 LOC unit test + ~40 LOC production code under
  `nomad_ch.rs::cleanup_orphans_at_startup` (extend the existing
  startup sweep to also enumerate `/sys/class/net`; pure-fs read,
  no privileged ops needed). Add the `inc_stranded_tap` metric
  mirroring `inc_vm_index_leak` (`metrics.rs:142`).
- **Severity IMPORTANT**: it's an observability gap, not a
  correctness one, but it's the OBSERVABILITY gap the stress run
  itself called out. P3 from the cluster report is the lens
  hand-off; test-cov should ride along when the counter lands.

## MINOR

### [R24-T4] `wake_machine_e2e` has 8 tests, not 13 as the task brief stated

- **Where**: `sandbox_pg_e2e.rs:5108` (mod start) to `:5690` (mod end).
  Counted via `awk` on `#[compio::test]` markers within the module
  range: **8 tests**. Names:
  `wake_machine_drives_snapshotted_to_ok` (`:5190`),
  `wake_machine_classifies_livez_failure` (`:5244`),
  `wake_machine_classifies_submit_failure` (`:5300`),
  `wake_machine_classifies_reserve_failure` (`:5346`),
  `wake_machine_terminal_clears_find_pending` (`:5394`),
  `wake_machine_gc_sweep_evicts_terminal_rows` (`:5449`),
  `wake_machine_terminal_overwrite_failed_to_ok_bumps_counter`
  (`:5530`, R23-I1),
  `wake_machine_terminal_overwrite_ok_to_failed_bumps_counter`
  (`:5617`, R23-I1).
- **Why MINOR**: the task brief's "13" figure looks like a count of
  ALL pg-gated wake-machine-related tests (`wake_jobs_*` CRUD tests
  in earlier modules also exercise the machine indirectly via
  `db.update_wake_job_state`). Within the dedicated `wake_machine_e2e`
  module, the figure is 8. Worth pinning the inventory so a future
  reviewer doesn't double-count.
- **Action**: at the top of `mod wake_machine_e2e` (`:5108`), add a
  one-line `//! 8 tests: 6 from C-7-LT-PR2 + 2 from R23-I1.` comment.
  ~1 LOC.

### [R24-T5] "Pre-existing failures" at HEAD claim is misleading — they only fail when pg is unreachable

- **Claim under audit (r23-I1 agent hand-off)**:
  `wake_machine_drives_snapshotted_to_ok` and
  `wake_machine_classifies_livez_failure` fail at HEAD. Root-cause
  hypothesis + fix path requested.
- **Verification**:
  - Both tests carry `#[ignore = "needs Postgres; C-7-LT-PR2 wake_
    machine …"]` (`sandbox_pg_e2e.rs:5189, 5243`).
  - r18 code-quality review (`docs/reviews/sandbox-snapshot-restore-
    code-quality-2026-05-25-r18.md:15,64`) explicitly states these
    fail ONLY because the local pg fixture at
    `postgres://postgres:zeroship@localhost:5440/zeroship` is
    unreachable in worktree environments where `docker compose up -d
    postgres` hasn't been invoked. Not a code bug.
  - At committed HEAD (`e6363fce`, after stashing the in-flight
    working-tree fix), `cargo test -p zeroship-sandbox --lib` returns
    `433 passed; 0 failed; 1 ignored`. No lib failures. The two
    "failing" pg-gated tests don't run under `--lib` at all (they
    live in `tests/sandbox_pg_e2e.rs`, an integration binary, and
    require both `--include-ignored` AND `SANDBOX_TEST_PG` reachability).
  - The R23-I1 closure entry in `deferred.md:1843` documents the
    R23-I1 fixer verified "both PASS in 6.2s under
    `--include-ignored --test-threads=1`" against a live local pg.
- **Root-cause**: there is no root-cause — the tests pass when pg
  is up. The R23-I1 hand-off was reporting environment state, not
  test code state.
- **Fix path**: nothing on the code side. R22-T4 already noted (and
  delisted) the CI-plumbing gap that means pg-gated tests are dev-
  validated only. This is the dual of that finding: the cost of
  not running pg-gated in CI is that "is it passing?" is per-
  operator and per-environment, not per-HEAD.
- **Severity MINOR**: a documentation / process clarity issue, not
  a code defect. If the audit brief is going to ask "does this
  test fail at HEAD?", the answer needs the precondition spelled
  out — and r24 backlog should not retain this as an open item.

### [R24-T6] Working-tree mid-fix for C-N-W1 (uncommitted) — 3 lib test failures + admin_handlers compile errors

- **Where**: working tree at audit time contained uncommitted
  changes to `admin_handlers.rs` (+184 LOC), `nomad_ch.rs` (+220
  LOC), `restore_handler.rs` (+83 LOC). The `restore_handler.rs`
  changes are the C-N-W1 controller-side preflight fix described
  above (R24-T2). The `admin_handlers.rs` changes are an unrelated
  role-scoped admin-auth refactor (read-only token vs full token)
  with compile errors (`E0282` type annotations needed at
  `admin_handlers.rs:210,249`).
- **Symptom**: at the working-tree-with-changes state, `cargo test
  -p zeroship-sandbox --lib` fails to compile because of the
  admin_handlers issues. After stashing only the admin_handlers
  changes, the build succeeds and 3 lib tests fail:
  `submit_restore_job_errors_when_nomad_500s`,
  `submit_restore_job_succeeds_when_nomad_returns_running`,
  `submit_restore_job_times_out_when_alloc_never_running`. Each
  panics at `restore_handler.rs:2905` (working-tree line numbers)
  with `workspace.img missing for sandbox …; controller-side parity
  check for driver preflight`. The fix's helper
  `stage_disk_image_preconditions` (`:2867`) IS called from each
  failing test (`:2905, :2933, :3050`) but the panic indicates the
  helper writes to one host_dir while the assertion reads from
  another — a fixture-vs-prod path mismatch.
- **Why MINOR**: this is an *in-flight* fix, not a HEAD state. At
  committed HEAD `e6363fce` the lib test suite passes 433/433.
  The mid-fix breakage is real but it's the author's WIP, not a
  test-cov regression.
- **Action**: the C-N-W1 PR (when it lands) must (a) match the
  helper's stage path against the assertion's host_dir resolution
  (look at how `submit_restore_job` derives `host_dir` from
  `self.cfg.host_state_dir.join(sandbox_id.simple().to_string())`
  — the helper's `cfg.host_state_dir.join(sandbox_id.simple())`
  needs the same `.to_string()` call), and (b) NOT mix the
  admin_handlers refactor into the same commit. The compile errors
  in admin_handlers (`E0282`) are blocking everything else and
  should be pulled out to a separate PR.
- **Severity MINOR**: WIP state, not landed gap.

### [R24-T7] Lib test count rose from 432 (r22) to 433 at HEAD — net +1, not +2

- **Where**: `cargo test -p zeroship-sandbox --lib` at HEAD
  `e6363fce` reports `433 passed; 0 failed; 1 ignored`. r22 trend
  table closed at 432. Delta: +1. R22-T1 (`b6c55d93`) was the
  expected single lib-test add (1 new `#[test]` in
  `restore_handler.rs::r12_i1_tests`).
- **Why MINOR**: r24's lib delta is +1, not +2. R23-I1's two new
  tests are pg-gated (in `tests/sandbox_pg_e2e.rs`, not `src/`),
  so they don't show in the lib count. r22's trend-table
  convention (lib-only) holds; the entries are correct, the brief
  just needs to acknowledge pg-gated movement separately.

## Trend table — carry from r22 with closure

| Cycle | sandbox lib | pg-gated | Δ lib | Δ pg | Notes |
|-------|-------------|----------|-------|------|-------|
| r17   | 373         | 74       | +29   | +9   | PR1+PR2 |
| r18   | 402         | 83       | +29   | +9   | PR2-FOLLOWUP + GATE-C2 + C-7-LT-1 |
| r19   | 424         | 87       | +22   | +4   | C-7-LT-2 + R17-S1 + R18-I1 + R19-C1 + R19-I1 |
| r20   | 426         | 87       | +2    | 0    | R19-I4 shims + R19-T1 widening |
| r21   | 428         | 87       | +2    | 0    | r17-Q3 DataIntegrity round-trip |
| r22   | 432         | 89       | +4    | +2   | R10-API4 + R20-C1 + R22-I1 counter |
| r23   | 432         | 89       | 0     | 0    | (no audit; smoke-r23 cluster cycle) |
| **r24** | **433**   | **91**   | **+1** | **+2** | R22-T1 parity + R23-I1 counter e2e |
| r9→r24 | +137       | +17      | —     | —    | wedge + sweep + sanitizer + probe + retry + drift + envelope + parity + counter-e2e |

Lib +1 since r22 (`R22-T1` field-list parity); pg +2 since r22
(`R23-I1` counter e2e × 2). PR `234c3bdf` (R23-I1) co-lands
counter-e2e only on pg; no lib companion — consistent with the
counter being a side-effect not a return value.

## Carry-forward table (r22 → r24)

| Tag | r22 status | r24 status | Note |
|-----|------------|------------|------|
| R22-T1 field-list parity | IMPORTANT, OPEN | **CLOSED** at `b6c55d93` | Single ~110 LOC test added to `restore_handler.rs::r12_i1_tests`. Note: closes the JSON-name parity invariant; does NOT close the host-fs-staging parity invariant (see R24-T2). |
| R22-T2 r21-A1 over-fulfilment | MINOR, OPEN | **CLOSED by R22-T1 superseding** | The 5-assertion `ch_plugin_restore_jobspec_populates_restore_from` test (`restore_handler.rs:3582`) is now redundant against R22-T1's field-set assertion. R22's "leave it alone" guidance holds. |
| R22-T3 R20-T1 retry-race pg test | MINOR, OPEN (3rd round) | **STILL OPEN (4th)** | No `insert_wake_job_retries_*` test landed. Concurrency-r23 didn't pick it up. Per r22 escalation rule, this is now **IMPORTANT** for r25. |
| R22-T4 CI pg plumbing | MINOR, DELISTED | n/a | Stays delisted; infra-lens territory. |
| R22-T5 multi-replica doc | MINOR, DELISTED | n/a | Stays delisted; code-as-doc suffices. |
| R21-T1 r17-Q3 DataIntegrity integration | MINOR, OPEN | **STILL OPEN (5th)** | Same pattern as R22-T3; not picked up. |
| R21-T2 host-fence constants | MINOR, OPEN | **DELIST** | Architecture has not ruled in 3 rounds; the constants are functionally encapsulated via `from_host_fence_timeout` (`restore_handler.rs` post-R20-I1 ADR extraction). Code-as-doc. |
| R21-T3 premise-check pattern | DELISTED | n/a | |
| R18-T5 persist=Some chain | MINOR, OPEN (14th) | **DELIST** | 14 rounds zero adoption; deferred.md:1056-1057 documents the closure via wake_machine paralleling the sync-path ladder. Carry-fatigue overrides; the actual gap is closed by C-7-LT-PR2's parallel coverage. |
| R18-T7 `probe_and_classify` sibling loops | MINOR, OPEN | OPEN, carry | Still 0 hits in test grep. |
| R18-T9 OS-thread detach pattern | MINOR, OPEN | OPEN, carry | 4 sites uncovered. |
| R19-T2 takeover_once lib coverage | MINOR, OPEN | OPEN, carry | R23-I1 closed adjacent (counter wiring) but not the loop itself. |
| R19-T3 R19-C1 claim-count metric | MINOR, OPEN | OPEN, carry | Co-file with R22-T3 escalation. |

## Would-have-caught analysis for the stress findings

### C-N-W1 (workspace.img cold-boot staging contract gap)

**What WAS caught**: R22-T1's field-list parity test (`b6c55d93`) was
the highest-leverage test-cov closure of the round. It caught the
**class** of bug where a Config field lands in one emitter but not the
other (`user_id` in r21-A1, `rootfs_source` in C-7-LT-12a). It did
**not** catch C-N-W1.

**Why R22-T1 did not catch C-N-W1**:
R22-T1 asserts the symmetric difference of *JSON Config key names*
between cold-boot's `build_nomad_job_json_with` and restore-path's
`build_restore_nomad_job_json` is exactly `{"rootfs_source"}`
(`restore_handler.rs:3837-3851`). C-N-W1 is not a JSON-shape bug — both
emitters correctly emit `disk` entries pointing at `<host_dir>/
workspace.img`. The bug is that the driver's NEW preflight requires the
target path to exist on disk **before** the alloc starts, and the
cold-boot controller path doesn't guarantee `fsync`-durable staging in
time. **R22-T1's invariant space is JSON; C-N-W1's invariant space is
filesystem.**

**What WOULD have caught C-N-W1 in-process** (no cluster):
A controller-side unit test, ~60 LOC, shaped like:
```
fn cold_boot_create_must_stage_workspace_img_before_submit() {
    let host_state = fresh_dir();
    let (nomad_addr, calls) = spawn_fake_nomad(...);
    let backend = NomadCHBackend::new(base_cfg(nomad_addr, host_state));
    // Inject a seam that fails create_ext4_image_if_missing or
    // skips it.
    backend.create(sid, "usr_test", "proj_test").await.expect_err(
        "missing workspace.img must reject before submit"
    );
    assert_eq!(calls.load(SeqCst), 0, "no Nomad RPC fired");
}
```
The in-flight working-tree fix lands this shape for the **restore**
path (`restore_handler.rs:3070, 3106`). The cold-boot mirror is not
yet in the tree.

### C-N-W2 (tap leak across vm_index reuse)

**What WAS caught**: nothing on the sandbox side. The tracking is
purely cluster-observable (`ip link` enumeration on worker hosts).

**Why no unit/integration test exists**: the tap lifecycle was
implicitly owned by `nomad-vm-wrapper.sh` (the bash wrapper),
specifically the `ip link set $TAP up` at `:250,727` and a now-
removed `trap` handler that did `ip link del` on script exit. The
Go driver (`nomad-driver-ch`) replicated `set up` but NOT `del`. No
controller-side code path inspects taps; no controller-side test
mocks the `/sys/class/net` surface.

**What WOULD have caught C-N-W2 in-process** (no cluster, no driver):
A controller-side integration test asserting `cleanup_orphans_at_
startup` enumerates `/sys/class/net/zsbx-nm-*` and either reclaims
or counts. Spec:
```
fn cleanup_orphans_at_startup_counts_stranded_taps() {
    let fake_net_root = fresh_dir();
    std::fs::create_dir(fake_net_root.join("zsbx-nm-4")).unwrap();
    std::fs::create_dir(fake_net_root.join("zsbx-nm-9")).unwrap();
    // Inject fake_net_root via cfg (new field) or env var
    let backend = NomadCHBackend::new(cfg);
    let pre = metrics::stranded_tap_value();
    backend.cleanup_orphans_at_startup().await.unwrap();
    let post = metrics::stranded_tap_value();
    assert_eq!(post, pre + 2);
}
```
~50 LOC test + ~40 LOC production code (enumerate dir, count, bump
metric). Requires a new `SANDBOX_NET_SYS_ROOT` config knob for
test injection.

### Why smoke-r23 (1 cycle) couldn't surface either bug

- **C-N-W1**: a single fresh-host CREATE has the file-system in a
  pristine state. The driver's preflight stat races the controller's
  truncate+mkfs; on a fresh host with no concurrent I/O, the stat
  wins. With 60 concurrent cycles (sequential per worker, 3 workers
  parallel) the stat loses 49/60 times. **Smoke's "fresh host" is
  the masking variable.**
- **C-N-W2**: a single CREATE → SNAPSHOT → WAKE → STOP cycle never
  reuses a vm_index. `VmIndexAllocator` hands out 1, then releases
  1 in the same cycle's STOP. No vm_index_K → K race because there's
  no K+1 cycle to race against. **Smoke's "single cycle" is the
  masking variable.**

**Generalisation**: smoke validates the happy-path traversal of the
state space; stress validates the *transitions between* state-space
points. Bugs that hide in the "B → A again with N=2 state retained"
shape are invisible to smoke by design. r24's structural recommendation:
**`docs/runbooks/sandbox-nomad-ch.md` should grow a "smoke vs. stress
matrix" describing which bug classes each gate catches.** (No test-
cov action; this is a runbook lens hand-off.)

## R23-I1 closure verdict — adequate

Counter sites:
- `wake_machine.rs:139-147` (`Phase::Ok` terminal write) → bumped.
- `wake_machine.rs:184-192` (`Phase::Failed` terminal write) → bumped.
- `wake_machine.rs:521-528` (`set_state` intermediate writes) → bumped.

Sibling search:
- `grep update_wake_job_state crates/sandbox/src/` returns only the
  three `wake_machine.rs` call sites (`:130, :175, :518`) plus the
  db.rs definition (`:3207`) and its docstring references. No
  sibling production caller.
- `db::claim_orphan_wake_for_recovery` (`db.rs:3358`) is the only
  other SQL UPDATE on `wake_jobs`; it filters non-terminal rows
  inside the WHERE clause and writes a takeover breadcrumb. It is
  NOT a `update_wake_job_state` caller and does NOT need the
  counter (it's the *producer* of the row state that the counter
  guards against, not a consumer of the guard).

Test coverage:
- `sandbox_pg_e2e.rs:5530` (`wake_machine_terminal_overwrite_failed_
  to_ok_bumps_counter`): pre-flip to Failed, drive happy-path. Counter
  must bump for both intermediate set_state (`:528`) AND terminal
  Ok (`:146`). Asserts `post >= pre + 1`. Sound.
- `sandbox_pg_e2e.rs:5617` (`wake_machine_terminal_overwrite_ok_to_
  failed_bumps_counter`): pre-flip to Ok, drive fail_livez. Counter
  must bump for intermediate set_state AND terminal Failed (`:191`).
  Asserts `post >= pre + 1`. Sound.

Verdict: **CLOSED. No sibling sites. No missed sites. Closure
discipline (Path B over Path A) was correct — moving the bump
into `db::update_wake_job_state` would have erased the
`attempted_state` context the WARN log carries.**

The only nitpick (deliberate, not a finding): the `>= pre + 1`
assertion is honest but loose. A future regression that adds a
NEW caller without the counter wire-up would not be caught — the
test would still pass on the existing 5-6 bumps. To pin tighter,
the test could snapshot the exact bump count via a label-aware
counter (`attempted_state=Ok` vs `attempted_state=Failed`). Not
worth landing now; the audit value is below the maintenance cost.

## Cross-lens consensus

- **R24-T1 (stress harness un-versioned)**: pure test-cov / infra
  lens. No other lens flagged it because the harness lives in
  cluster-lens territory; test-cov picks it up because the
  reproducibility gap maps to "what does HEAD validate?".
- **R24-T2 (cold-boot workspace.img controller test)**: closes the
  cold-boot half of what api-surface-r21 R21-API2 + test-cov-r22
  R22-T1 caught for the JSON-emitter half. Three-lens convergence
  on the same root pattern: cold-boot and restore have two parallel
  paths and the contract between them is enforced only by source-
  order / human discipline, not by tests. Hand-off to architecture
  r24+ to formalise.
- **R24-T3 (tap-leak observability)**: aligns with T-8b-stress P3
  (`cluster-T8b-stress.md:165`). Three-lens hand-off: test-cov asks
  for the counter test; performance lens may want the
  enumeration-cost budget; security lens may want a TOCTOU note on
  reading `/sys/class/net` from a controller that may not have
  CAP_NET_ADMIN. None of those block the test from landing.
- **R23-I1 closure**: code-quality r24 should re-confirm. The Path B
  decision (bump at caller, not at db) is an observability
  architecture call that test-cov endorses but architecture owns.

## Lens hand-off

**To api-surface r24**: R24-T2 closes a sibling of R21-API2 / R22-T1.
Recommended PR title "controller-side workspace.img preflight + cold-
boot symmetric test (R24-T2)". The in-flight working-tree restore-path
fix is a strong template; mirror it on `nomad_ch.rs::create()`.

**To architecture r24**: R24-T1 — formalise the stress-harness
versioning policy. Either commit the script + a CI lane that runs
it against `localhost` or document explicitly that the canonical
copy lives at `gs://…` with a release-pinning protocol. Either is
defensible; the current "Schrödinger's harness" is not.

**To code-quality r24**: R23-I1 closure verdict for cross-confirm.
Path B (caller-side bumps) is the right architecture; test pair at
`sandbox_pg_e2e.rs:5530, 5617` is the right coverage.

**To concurrency r24**: R22-T3 (R20-T1) retry-race pg test, now 4
rounds open. r22 set the escalation rule: "one more round of slip
without a clear blocker and this escalates back to IMPORTANT at r23".
Concurrency-r23 did not greenlight or downgrade. R25 backlog has
this as IMPORTANT.

**To test-cov r25 backlog** (~250 LOC total):
1. **R24-T1** commit `snapshot_stress.py` + minimal invariant test —
   ~120 LOC (script ~90 + test ~30). CRITICAL.
2. **R24-T2** cold-boot `workspace.img` preflight unit test — ~60 LOC.
   IMPORTANT.
3. **R24-T3** stranded-tap counter + test — ~80 LOC (production ~40
   + test ~50). IMPORTANT.
4. **R22-T3** (carry, now IMPORTANT) — R20-T1 retry-race pg tests
   — ~80 LOC.
5. **R21-T1** (carry) — r17-Q3 DataIntegrity integration — ~50 LOC.

Delisted this round: R21-T2 (host-fence constants — architecture
silent for 3 rounds), R18-T5 (persist=Some chain — 14 rounds, gap
closed by parallel wake_machine path).

## Notes for r24

- **The big shift this round**: test-cov was first round in 5 to
  surface a CRITICAL (R24-T1). The crate-side test inventory is now
  mature (433 lib + 91 pg-gated) and the marginal gaps are in
  **adjacent tooling** (harnesses, observability counters, contract
  tests across worktrees), not in the crate itself.
- **R22-T1 was correctly prioritised** but C-N-W1 proved the
  *invariant space* matters as much as the *invariant assertion*.
  A field-list parity test catches field-list bugs; a host-fs
  staging parity test would have caught C-N-W1. Future cross-emitter
  contract tests should pick the **highest-leverage invariant**, not
  whatever's easiest to assert as JSON.
- **R23-I1 is the textbook good closure**: scope was clear,
  Path B was the right architectural call, the test pair drives the
  full machine end-to-end with the right pre-condition. Carry the
  pattern forward when concurrency-r24+ closes R22-T3.
- **The smoke → stress observation generalises**: any test that
  reads "validate the contract in a single transition" is
  structurally blind to "what happens when transitions accumulate
  state". This is the dual of unit-vs-integration; call it
  "one-cycle-vs-many-cycle". Both gates are needed.
- **Highest-leverage closure for r25**: R24-T1 (commit the
  stress harness). ~120 LOC; closes the reproducibility CRITICAL;
  unlocks the "did the harness regress?" question that the next
  driver-v13 / controller-v33 re-stress cycle will need answered.
