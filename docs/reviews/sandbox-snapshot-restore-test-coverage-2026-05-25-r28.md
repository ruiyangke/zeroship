# Sandbox/snapshot-restore — test-coverage r28 review

Date: 2026-05-25 (UTC). HEAD at audit: `885e2abb` (worktree
tip; T-8b-stress-r6 RED review docs landed). Option C Phase 2
wire-schema + counters in flight cross-worktree at
`nomad-driver-ch` `032da940` / `42a37265`. Round 28. Prior:
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-
25-r27.md` (HEAD `01288c18`).

Working tree at audit: 2 modified files. `crates/sandbox/src/
config.rs` adds `driver_stages_disk_images: bool` (Phase 2
controller-side flag). `cargo test -p zeroship-sandbox --lib
--no-run` is RED with 5 `missing field driver_stages_disk_
images in initializer of config::SandboxConfig` errors in
test fixtures. This is the in-flight Phase 2 diff, not a
test regression. Per brief guardrail (NO code edits): the
fixture-update half hasn't been committed yet; observed,
not touched.

## TL;DR

- **Stress-r6 RED 3/60 = 3rd consecutive RED at the same
  rate.** r4-A reap-wait + r5-A OFD probe — both layered
  "wait harder" attempts failed. r5-A specifically failed
  with a production EBADF bug the driver's 140-PASS test
  suite did not catch because the test fixture used a
  happy-path `os.CreateTemp` file, NOT the post-CH-exit
  kernel state the predicate observes in production. Per
  arch r27-A3 abort: Option C structural migration in
  flight.

- **NEW [R28-DISCIPLINE] (HIGHEST LEVERAGE)**: r5-A is the
  canonical example of a test-discipline failure mode this
  series has not previously named. Stated as a rule:

  > **A test fixture for a kernel-state-observing predicate
  > MUST reproduce the production state the predicate
  > observes. Fresh `os.CreateTemp` / `std::fs::write` files
  > DO NOT reproduce post-CH-exit / post-fork / post-mount-
  > unshare state. Predicates over file descriptors, OFD
  > locks, deferred `__fput` workqueue, inode references,
  > namespace lifetimes need fixtures that construct that
  > state explicitly (subprocess open-then-exit, fork +
  > mmap, namespace unshare).**

  C-7-LT-12 (hardlink-vs-copy at `50fb987d`) is the second
  instance of this pattern in 28 rounds. r28 NAMES it
  because Option C Phase 2 is about to land 4-6 NEW such
  predicates (`mkfs.ext4` post-condition, ext4 magic at
  byte 1080, `fsync_dir` ordering, `runDir` lifecycle vs
  Nomad reap, disk-image idempotency under operator
  restart) — without the rule, the next stress round risks
  repeating r5-A.

- **NEW [R28-PHASE2-TEST-PLAN]**: minimum lib/driver test
  suite for driver-side `stageDiskImages` (forward-looking
  — the op itself has not landed; only the wire-schema
  field + counters have, at `42a37265` + `032da940`). 6
  cases per ADR Phase 2 exit criterion ("no failure mode
  introduced"):

  1. **Happy path × N disks**: stage workspace + home;
     assert both exist, both >= size_gb, both have ext4
     magic `0x53ef` at byte offset 1080 (the
     load-bearing post-condition, STRONGER than the
     controller-side `size > 0` predicate).
  2. **mkfs.ext4 failure** (mocked subprocess): typed
     error contains verbatim mkfs stderr + disk-kind tag;
     `start_task_stage_failures_total{kind=workspace}`
     bumped.
  3. **ENOSPC on truncate**: tmpfs+size=1MB fixture;
     error contains "ENOSPC" or "No space left"
     verbatim.
  4. **EACCES on parent dir**: 0500 parent; error
     contains "permission denied" + parent path.
  5. **Idempotent re-stage**: operator restarts task
     before reap; pre-stamp sentinel bytes; assert
     sentinel survives. Mirrors controller-side
     `create_ext4_image_if_missing_skips_when_file_
     exists`.
  6. **Default-flag pinning**: with `StageDiskImages=
     false` (Phase 2 default), the op is NOT called;
     legacy path engaged. Load-bearing back-compat
     contract for the migration window — test it by
     name.

  ~150 LOC total. Land WITH the `stageDiskImages` commit,
  not after.

- **NEW [R28-PHASE2-OBS-COUNTERS]**: counter validation
  tests must include BOTH seam-driven branch coverage AND
  production-state validation. The r5-A precedent:
  `destroy_task_lock_held_total` test
  (`TestDestroyTask_ProceedsOnLockHeldBudgetExhausted`)
  used `SetTryAcquireOFDLockForTest` to drive the
  budget-exhaust branch. Test PASSED. Production: counter
  rose `16→17→18→19→20` because EBADF (not EAGAIN)
  triggered the same branch. Counter test validated the
  COUNTER, not the PREDICATE. Phase 2's new counters
  (`start_task_stage_total`, `start_task_stage_failures_
  total` at `032da940`) need (a) seam branch coverage +
  (b) production-state validation BEFORE Phase 4
  default-flip.

- **R25-T2 RESURRECTED to IMPORTANT from r27 MINOR
  demote.** Phase 3 cutover changes the predicate shape:
  controller-side `try_create` becomes manifest-validation-
  only (no `create_ext4_image_if_missing`, no stat). The
  defensive invariant "preflight rejects missing local
  image" becomes vacuously true. The new ask: "preflight
  rejects malformed StagingManifest." See [R28-T1] body.

- **R26-T1 verbatim-msg exit tests — 7TH CYCLE, shape
  shifted.** `extract_failed_task_event_msgs` has 9 entry
  tests (`nomad_ch.rs:4493-4805`) + 0 exit tests (verified
  at HEAD `885e2abb`). Pre-Option-C wedge text = CH
  `rootfs.img AlreadyLocked`. Post-Option-C, the WAKE
  failure class shifts to Phase 2/3 staging errors
  (`mkfs.ext4: ENOSPC`, `manifest validation: empty disks`).
  The propagation chain (driver-msg → sanitize → pg → admin)
  is identical; the failure class is different. **The
  7-round-open test is now MORE load-bearing, not less.**

- **In-flight build state**: lib `cargo test --no-run` RED
  at HEAD due to fixture lag on `driver_stages_disk_
  images`. Brief guardrail: not touching. The fixture
  patch is a 5-line `..,driver_stages_disk_images: false`
  addition across 5 test sites. Documented as fixture-lag,
  NOT a test regression.

## CRITICAL

None.

## IMPORTANT

### [R28-DISCIPLINE] [NEW] Kernel-state predicate fixtures must reproduce production state

**Statement:**

> Before claiming a kernel-guarantee predicate is satisfied,
> the test fixture must reproduce the production state the
> predicate observes.

**Where the rule applies:**

- File-descriptor lifecycle: `fcntl`, `flock`,
  `F_OFD_SETLK`, sequences of `open + close + reopen`,
  `dup + dup2`. The fd's kernel state is bounded by the
  `struct file`, not by the caller's stack frame.
- OFD lock predicates: lock outlives final `close` per
  deferred `__fput`. A fresh tempfile has no leaked
  `struct file`; production CH-exit does.
- Inode + dentry lifecycles: `unlink + reopen`, `rename +
  reopen`, `link + close`.
- Namespace / mount predicates: `mount`, `unshare`,
  `pivot_root`, network-namespace operations.
- `delayed_fput` / `delayed_work` predicates: "is the
  kernel finished with this resource" — fixture MUST
  construct the workqueue-pending state, typically by
  forking a subprocess that opens-and-exits.

**Where the rule was violated:**

- **r5-A `F_OFD_SETLK` probe**: driver test fixture used
  `SetTryAcquireOFDLockForTest` seam, bypassing
  `realTryAcquireOFDLock` (the only path that actually
  calls `unix.Open` + `unix.FcntlFlock`). 140 tests
  PASSED. 60 production cycles FAILED with EBADF on
  attempt 1.
- **C-7-LT-12 hardlink-vs-copy** (earlier, same pattern):
  fixture used a happy-path source; production needed
  cross-filesystem link / copy fallback.

**How to apply (operationally):**

1. **Name the kernel state in the predicate docstring**:
   "this predicate observes that no `struct file`
   references the inode at `path` with F_WRLCK held."
2. **List the production-state-construction recipes**:
   subprocess open + flock + exit (deferred __fput);
   fork + mmap (shared inode references); namespace
   unshare (per-ns dentry refs).
3. **Land BOTH a seam branch-coverage test AND a
   production-state validation test.** The seam test is
   acceptable for fast CI; the production-state test is
   the load-bearing validation. **Seam-only = R5-A
   failure mode.**
4. **For predicates without a reproducible recipe**,
   document the gap and cite the stress test as the only
   validation oracle.

**Cost analysis (r5-A retrospective):**

- Cost of NOT having the rule: 3 stress rounds × ~$2/run
  hardware + ~8 hours investigation + Option C
  structural pivot ≈ $300 human-hours-equivalent (not
  counting the 2-day wedge).
- Cost of HAVING the rule: ~40 LOC per predicate × ~6
  Phase 2 predicates ≈ 240 LOC. ~2 hours engineer time.

The rule pays for itself in ~1 prevented stress-RED cycle.

**Severity IMPORTANT.** Land in `docs/decisions/2026-05-25-
kernel-state-surface-inventory.md` (sibling discipline
ADR) OR its own discipline note before the next Phase 2
predicate commit lands. **Highest-leverage r28
deliverable.**

### [R28-PHASE2-TEST-PLAN] [NEW] Driver-side `stageDiskImages` minimum 6-case suite

**Where**: forward-looking, no code yet at HEAD. Phase 2
counters and wire field landed at `nomad-driver-ch`
`032da940` + `42a37265`; the `stageDiskImages` op is the
next commit per ADR Phase 2 plan.

**Required test cases when the op lands** (per ADR exit
criterion "no failure mode introduced"):

1. **Happy path × 2 disks** (~30 LOC). Stage workspace +
   home. Assert: both files exist; both >= 20 GiB; both
   have ext4 magic `0x53ef` at byte 1080. Counters:
   `start_task_stage_total +1`,
   `start_task_stage_failures_total` unchanged. **Critical
   fixture discipline (R28-DISCIPLINE)**: verify bytes
   not just `os.Stat`. A zero-byte file passes `Stat`
   but is the failure mode the controller-side
   `create_ext4_image_if_missing_skip_path_rejects_zero_
   byte_file` already catches.
2. **mkfs.ext4 failure** (mocked subprocess) (~25 LOC).
   Inject the subprocess runner via a `mkfsRunner func(
   args []string) (string, error)` field. Returned error
   must contain verbatim mkfs stderr + disk-kind tag.
3. **ENOSPC on truncate** (~30 LOC). Linux-only;
   tmpfs+size=1MB fixture. Error must contain "ENOSPC"
   or "No space left" verbatim.
4. **EACCES on parent dir** (~20 LOC). `0500` parent;
   error contains "permission denied" + path.
5. **Idempotent re-stage** (~30 LOC). Pre-stamp
   sentinel bytes. Assert sentinel survives, counter
   bumps as `success` not `fail`. Mirrors `nomad_ch.rs:
   5532` controller-side idempotency contract.
6. **Default-flag posture** (~15 LOC). `StageDiskImages:
   false` → op NOT called (seam-count invocations);
   legacy path engaged; counter unchanged. **Load-bearing
   migration-window guardrail.**

Total ~150 LOC. Land WITH the `stageDiskImages` commit.
**Severity IMPORTANT.**

### [R28-PHASE2-OBS-COUNTERS] [NEW] Counter validation = branch coverage + production state

**Where**: Phase 2 counters at `nomad-driver-ch/ch/metrics.
go` (`032da940`): `start_task_stage_total`,
`start_task_stage_failures_total`.

**The r5-A precedent**: counter test
`TestDestroyTask_ProceedsOnLockHeldBudgetExhausted` used
the `SetTryAcquireOFDLockForTest` seam to drive the
budget-exhaust branch. Test PASSED. Production: counter
rose monotonically `16→17→18→19→20` — but only because
EBADF was triggering the budget-exhaust branch every
time. The counter validated the BRANCH structure
(seam-driven); it did NOT validate the PREDICATE.

**Required test posture (BOTH shapes):**

- **Seam-driven branch coverage** (fast, CI-friendly):
  ~20 LOC per branch. Asserts "GIVEN scripted result
  sequence, counter bumps N times." Acceptable for unit
  CI signal.
- **Production-state predicate validation** (slow, may
  need root/subprocess): ~40 LOC per counter. Asserts
  "GIVEN production-shaped fixture, real syscall path
  executes, counter reflects actual outcome." Load-
  bearing for Phase 4 default-flip.

**Severity IMPORTANT.** Phase 4 default-flip cannot rely on
counter-pass-on-seam-test as evidence of correctness.
~80 LOC of production-state tests across the two
counters.

### [R28-T1] [NEW] R25-T2 resurrected: Phase 3 controller-side manifest validation

**Where**: prior R25-T2 asked for `submit_nomad_job_
rejects_missing_workspace_img` mirroring `restore_handler.
rs:3362`. r27: demoted MINOR (stress-r4 CREATE 60/60
closed cluster motivation).

**What r28 changes**: Phase 3 cutover (per ADR) makes
controller-side `try_create` validation-only — no
`create_ext4_image_if_missing`, no `mkdir`, no
`assert_disk_image_present`. The invariant "preflight
rejects missing local image" becomes vacuously true.

**New ask**: validate the MANIFEST the controller emits:

- `try_create_rejects_manifest_with_empty_disks_array()`
- `try_create_rejects_manifest_with_invalid_kind()` —
  not in `{workspace, home, rootfs}`.
- `try_create_rejects_manifest_with_zero_size()`
- `try_create_emits_valid_manifest_under_driver_stages_
  disk_images_true()` — sets the flag, asserts job-spec
  carries the typed manifest.

~100 LOC. Land WITH Phase 3 controller-cutover commit.
**Severity IMPORTANT (resurrected from MINOR).**

### [R28-T2] [CARRY] R26-T1 verbatim-msg exit tests — 7th cycle, shape shifted

**Where**: chain unchanged from r26/r27. 9 entry tests at
`nomad_ch.rs:4493-4805`; 0 exit tests (`Grep "extract_
failed_task_event_msgs|disk\[" crates/sandbox/tests/`
returns 1 unrelated match).

**What shifts under Option C**: Phase 2/3 introduces a NEW
failure class:
- Phase 2 driver-side: `mkfs.ext4 failed: ENOSPC`,
  `permission denied`, `module 'ext4' not loaded`.
- Phase 3 controller-side: `manifest validation: empty
  disks array`, `manifest validation: kind not in
  {workspace,home,rootfs}`.

Propagation chain (driver-msg → sanitize → pg → admin
`message` field) is IDENTICAL; failure texts are
DIFFERENT. The 7-round-open exit test is now MORE
load-bearing — it's the only oracle for whether Phase 2/3
diagnostic egress works.

**What WOULD close it** (carried unchanged):
- pg-gated at `sandbox_pg_e2e.rs:5530`: stub backend
  returns `"stage_disk_images: mkfs.ext4 workspace:
  ENOSPC"`; assert `error_message` contains "mkfs.ext4"
  AND "ENOSPC" AND "workspace". ~35 LOC.
- admin-handler at `sandbox_admin_e2e.rs`: seed
  `wake_jobs` row pre-redacted; hit endpoint;
  assert 200 body `message` substrings. ~40 LOC.

**Severity IMPORTANT.** 7-round carry, no demotion possible.

### [R28-T3] [NEW] Phase 2 driver-side post-condition mirror — ext4 magic check

**Where**: 5 controller-side tests at `nomad_ch.rs:5607-
5676` pin `assert_disk_image_present`: normal / missing /
directory / zero-byte / parity. The controller predicate
checks `len > 0`. Driver-side Phase 2 needs a STRONGER
mirror.

**Required driver-side mirror predicate**: file exists +
non-zero size + NOT a directory + **ext4 magic `0x53ef` at
offset 1080** (mkfs.ext4 post-condition). The magic check
is the gap the controller-side predicate has: a truncated-
but-mkfs-failed file passes `size > 0` but the magic is
absent. Driver-side is the right layer to close it.

~80 LOC. Land WITH Phase 2 `stageDiskImages` commit.
**Severity IMPORTANT.**

### [R28-T8] [NEW] Driver test-suite shape that would have caught r5-A

**Where**: 140-PASS driver suite (`nomad-driver-ch/tests/
stop_task_test.go`); 3 r5-A tests use
`SetTryAcquireOFDLockForTest` seam exclusively.
`realTryAcquireOFDLock` at `stop_task.go:423` is the only
path making the actual `unix.Open` + `unix.FcntlFlock`
syscalls. Production EBADF lives in this path. Tests
asserted branch structure of `pollAcquireOFDLock`; tests
did NOT assert syscall structure of
`realTryAcquireOFDLock`.

**The shape that WOULD have caught r5-A**:

```go
func TestRealTryAcquireOFDLock_PostSubprocessExit(t *testing.T) {
    tmp, _ := os.CreateTemp("", "ofd-test-")
    tmp.Close()
    defer os.Remove(tmp.Name())

    // Fork a subprocess: open + flock + exit-without-close.
    // The subprocess's `struct file` goes into delayed_fput
    // purgatory after the subprocess exits.
    cmd := exec.Command(os.Args[0], "-test.run=TestHelper_HoldLockAndExit", tmp.Name())
    cmd.Env = append(os.Environ(), "TEST_HELPER=1")
    if err := cmd.Run(); err != nil { t.Fatalf("%v", err) }

    result, err := realTryAcquireOFDLock(tmp.Name())
    if err != nil {
        // The r5-A EBADF surfaces HERE — in a unit test
        // with a debugger attached. 1-line fix. No $6
        // stress cycle.
        t.Fatalf("realTryAcquireOFDLock returned err: %v", err)
    }
    if result != ofdLockProbeAcquired && result != ofdLockProbeBusy {
        t.Errorf("unexpected result: %v", result)
    }
}
```

**This is R28-DISCIPLINE applied to the r5-A predicate**:
the fixture constructs the production state (subprocess
exit → deferred `__fput`) and runs the probe against it.

**Severity IMPORTANT.** Carries forward to ALL Phase 2/3
driver-side kernel-state predicates: `mkfs.ext4` ordering
vs ext4 journal, `runDir` unlink during in-flight stage
(Nomad reap race), subprocess-spawn-then-cleanup-window
(CH process exit timing). ~40 LOC per predicate × ~6
predicates = ~240 LOC across Phase 2/3.

### [R28-T5] [NEW] StagingManifest wire-schema parity test

**Where**: forward-looking. Phase 2 commit `42a37265` adds
`StageDiskImages bool` to driver TaskConfig. The next
commit (per ADR Phase 3) is the typed `StagingManifest`
shape — emitter on the controller side, parser on the
driver side.

**Required pattern** (per R22-T1 / R26-T2 discipline,
which landed `node_affinity_constraints_parity_between_
cold_boot_and_restore_emitters` at `d71f1a8c`):

- `staging_manifest_emit_parse_round_trip()` — controller
  emits, driver parses, assert deep-equality.
- `staging_manifest_unknown_disk_kind_rejected_by_parser()`
- `staging_manifest_empty_disks_array_rejected()` — both
  sides.

~50 LOC. Land WITH Phase 3 controller-side emission
commit. **Severity IMPORTANT.**

### [R27-T3-CARRY] R26-T4 partial-closure: boot-time composition contract unpin

**Where**: `lib.rs:670-698` — boot path's
`fetch_local_nomad_node_id().await { Ok | Err → WARN +
counter + None + continue }` composition. Parser (5
cases) + counter monotonicity tested individually;
composition not.

**Status under Option C**: survives Phase 3 unchanged
(boot-time orthogonal to staging locality). Carries.

~30 LOC. **Severity IMPORTANT.**

## MINOR

### [R28-T4] [REFRAMED] R27-T1 lock-collision contract → regression guard under Option C

R27-T1 asked for "STOP → next WAKE for same sandbox_id MUST
succeed; no rootfs.img lock contention" lib test. Under
Option C Phase 3, the rootfs.img cross-alloc lock surface
COLLAPSES (per-alloc fresh hardlink; no shared OFD lock).
R27-T1's value shifts from "diagnostic test for active
wedge" to "regression guard Option C didn't reintroduce
the surface." Lands as part of Phase 4 stress-validation
artifacts, not pre-Phase-2. **Severity reframed from
IMPORTANT to MINOR.** ~75 LOC.

### [R28-T6] [NEW] Sweep tests creator-agnostic under Option C Phase 3

Under Phase 3, the sweeper continues to reap dirents in
the same `<HOST_STATE_DIR>/<sandbox_id>` topology, same
mtime + DB-join gates, but the CREATOR shifts controller
→ driver. The 13 R25-T4 helper tests gate on dirent shape;
creator-identity is IMPLICIT. 1 explicit defensive test
asserting creator-agnostic gate behavior (`classify_host_
dir_entry_treats_driver_staged_and_controller_staged_
dirents_identically`) would document the invariant. ~15
LOC + 1 docstring. **Severity MINOR.**

### [R28-T7] [NEW] Phase 3 cutover must not orphan `create_ext4_image_if_missing` tests

5 lib tests at `nomad_ch.rs:5520-5676` pin the controller-
side helper. Under Phase 3 the helper has no production
caller. The Phase 3 cutover commit must either: (a) keep
helper + tests (defensible if helper survives in dev
tooling); OR (b) delete both in one commit. The wrong
outcome: helper kept, tests kept, NO production caller —
CI green because the post-condition has no consumer.
**Severity MINOR.** Process-discipline.

### [R28-S1] [NEW] Phase 2 driver-side path leakage in sanitizer

`wake_machine.rs:1063` `strip_filesystem_paths` whitelist
preserves `/opt/nomad/...` verbatim. Phase 2 driver
errors will surface `/opt/nomad/data/alloc/<alloc_id>/ch/
local/workspace.img` containing bare Nomad-flavored UUID
(36-char hyphenated, NOT typed-prefix form so
`strip_typed_ids` at `wake_machine.rs:1118` misses it).
Same shape as R27-S1 (cloud-hypervisor.sock UUID leak).
~10 LOC widening. Defer until Phase 2 stress surfaces
actual leaks. **Severity MINOR.**

### [R28-T6-LIB-CARRY] R27-T6-LIB sweep orchestration body — STILL OPEN

`sweep.rs:run_host_dir_gc_once` — 1 pg-gated test;
orchestration body's count semantics un-pinned. ~30 LOC
pg-gated test. **Severity MINOR.** 2nd-round carry.

### [R27-T5] [DEMOTED] STOP HTTP timing semantics — moot under Option C

r27 IMPORTANT (active stress-r4 wedge): "STOP returns 200
before CH process exits + lock release." Under Option C
Phase 3, the cross-alloc lock contention surface
COLLAPSES; the STOP timing contract becomes a regression
guard with low urgency. Demote to deferred backlog.

### [R25-T6-CLOSE] R25-T6 UTF-8 truncation safety — CLOSED at `dfccd049`

r27-M2 char-boundary truncation closed the 8th-round
carry. Full verification pending the in-flight fixture-
update commit unblocking `cargo test --lib`.

## Carry-forward table (r27 → r28)

| Tag | r27 status | r28 status |
|-----|------------|------------|
| R25-T1 stress harness polling | IMPORTANT, OPEN (3rd) | **OPEN (4th)** |
| R25-T2 cold-boot preflight | MINOR (5th) | **IMPORTANT, RESURRECTED → R28-T1** |
| R25-T3 multi-cycle vm_index race | IMPORTANT, OPEN | **OPEN (4th)** |
| R25-T6 UTF-8 truncation | OPEN (8th) | **CLOSED at `dfccd049`** |
| R26-T1 verbatim-msg exit | IMPORTANT (6th) | **IMPORTANT (7th) → R28-T2** |
| R26-T6 placement-audit cluster | IMPORTANT | **IMPORTANT (3rd carry)** |
| R27-T1 stress-r4 lock-collision | IMPORTANT | **MINOR, REFRAMED → R28-T4** |
| R27-T3 boot-failure composition | IMPORTANT | **IMPORTANT, CARRY** |
| R27-T4 read_snapshot_row | MINOR | **MINOR, CARRY** |
| R27-T5 STOP HTTP timing | IMPORTANT | **MINOR, DEMOTED (Option C moots)** |
| R27-T6-LIB sweep orchestration | MINOR | **MINOR, CARRY** |
| R27-S1 sanitize bare-UUID | MINOR | **MINOR → R28-S1** |
| **NEW R28-DISCIPLINE** | — | **IMPORTANT, NEW** |
| **NEW R28-PHASE2-TEST-PLAN** | — | **IMPORTANT, NEW** |
| **NEW R28-PHASE2-OBS-COUNTERS** | — | **IMPORTANT, NEW** |
| **NEW R28-T1** | — | **IMPORTANT, NEW** |
| **NEW R28-T3** | — | **IMPORTANT, NEW** |
| **NEW R28-T5** | — | **IMPORTANT, NEW** |
| **NEW R28-T8** | — | **IMPORTANT, NEW** |
| **NEW R28-T6** | — | **MINOR, NEW** |
| **NEW R28-T7** | — | **MINOR, NEW** |

## Bundle-specific would-have-caught analysis

### Does r4-A + r5-A + r6 close the stress wedge?

**No.** Three consecutive RED-at-3/60. Per ADR abort
criterion the diagnostic ladder is exhausted; Option C is
chosen.

### Would [R28-DISCIPLINE] have caught r5-A?

**Yes.** A production-state-driven test of
`realTryAcquireOFDLock` against a post-subprocess-exit
file fixture would have exercised the syscall path the
seam bypassed. EBADF surfaces in a unit test; fix is a
1-line patch.

### Does Option C Phase 2's observability-first sequencing make sense?

**Yes, IF accompanied by [R28-DISCIPLINE].** Phase 2
sequencing: wire-schema + counters first, op next,
default-flip in Phase 4 after stress validation. This is
correct — IF counter validation tests follow [R28-
PHASE2-OBS-COUNTERS] (production-state, not seam-bypass).
Otherwise the counters validate branches (seam test
passes) but not predicates (r5-A precedent).

## What stress-r-Option-C will exercise that unit tests miss

1. **mkfs.ext4 + fsync ordering under load**. Phase 2
   stages disks per-alloc; under 3 workers × 20 cycles,
   ext4 journal contention may surface delays.
2. **Per-alloc `runDir` lifecycle vs Nomad reap**. Driver
   mid-write race with `rm -rf` from Nomad alloc cleanup.
   Hard to unit-test without root/namespace tricks.
3. **Cross-disk-kind sequencing**. Failure mid-sequence
   must not leave partial state next retry can't recover
   from.
4. **Default-flag flip timing**. Phase 4 flip is one-way;
   migration window depends on every worker carrying the
   same default. Cluster pins rollout coherence.

## Lib tests gating cutover

Lib count at HEAD `885e2abb`: **uncountable this round**
(`cargo test --lib --no-run` RED on in-flight
`driver_stages_disk_images` fixture gap). Pg-gated count
unchanged at 91 (inspection of `#[ignore]` annotations).

**Phase 4 cutover gate sufficiency**: **INSUFFICIENT**
without:

- R28-DISCIPLINE adopted as documented rule.
- R28-PHASE2-TEST-PLAN 6 cases landed with `stageDisk
  Images` op.
- R28-PHASE2-OBS-COUNTERS production-state counter
  validation landed.
- R28-T1 Phase 3 manifest validation tests landed.
- R28-T2 (R26-T1 7th carry) verbatim-msg exit tests
  landed.
- R28-T3 driver-side post-condition mirror (ext4 magic)
  landed.
- R28-T5 wire-schema parity test landed.
- R28-T8 production-state driver fixture pattern adopted
  across Phase 2 predicates.

Eight asks; 3 NEW for r28; rest carry/reshape. ~620 LOC
across Phase 2/3 commits.

## To test-cov r29 backlog (~620 LOC total)

1. **R28-DISCIPLINE** — discipline rule documented. ~80 LOC
   docs. **HIGHEST LEVERAGE.**
2. **R28-PHASE2-TEST-PLAN** — 6-case `stageDiskImages`
   suite. ~150 LOC.
3. **R28-PHASE2-OBS-COUNTERS** — production-state counter
   validation. ~80 LOC.
4. **R28-T1** — Phase 3 manifest validation. ~100 LOC.
5. **R28-T2** — verbatim-msg exit (R26-T1 7th carry). ~75
   LOC.
6. **R28-T3** — Phase 2 post-condition mirror + ext4 magic.
   ~80 LOC.
7. **R28-T5** — StagingManifest wire-schema parity. ~50
   LOC.
8. **R28-T8** — driver production-state fixture pattern.
   ~150 LOC NEW (after de-dup with T-3 + PHASE2-TEST-PLAN).
9. **R27-T3** — boot-failure composition test. ~30 LOC.
10. **R28-T4** — lock-collision regression guard
    (reframed). ~75 LOC. Phase 4/5.
11. **R28-T6** — sweep creator-agnostic. ~15 LOC.
12. **R28-T7** — Phase 3 cutover test-orphan discipline.
    Trivial.
13. **R28-S1** — Phase 2 path sanitizer widening. ~10 LOC.
14. **R25-T1** — stress-harness invariant (4th carry).
    ~120 LOC.
15. **R25-T3** — multi-cycle vm_index race (4th carry).
    ~80 LOC Go.
16. **R22-T3** — retry-race pg test (8th carry). ~80 LOC.
17. **R27-T6-LIB** — sweep orchestration body. ~30 LOC.
18. **R27-T4** — `read_snapshot_row` pg-gated. ~40 LOC.

Delisted this round: R25-T6 (CLOSED at `dfccd049`); R27-T5
(DEMOTED — Option C moots the surface).

## Notes for r29

- **The shift this round**: r28 is the round where the
  cluster lens (stress-r6 RED 3rd consecutive) and the
  architecture lens (ABORT to Option C) converged on a
  structural pivot, AND the test-coverage lens
  RETROSPECTIVELY identified the META-FAILURE mode (r5-A
  EBADF on happy-path fixture) that contributed to the
  abort. r28's deliverable is NOT a test count delta;
  it's a NAMED test-discipline rule that the ~6 Phase 2
  predicates must follow.
- **The shift in R25-T2**: r25 IMPORTANT (catch cluster
  bug pre-stress) → r27 MINOR (cluster bug closed by
  r3-A) → r28 IMPORTANT (Phase 3 cutover changes the
  predicate shape — "preflight image" → "preflight
  manifest"). The ask survives the architectural pivot,
  predicate evolves.
- **The shift in R26-T1**: 7th round open; under Option C
  the failure-class shifts from CH lock contention to
  Phase 2 staging errors. Exit-side test is MORE
  load-bearing not less.
- **The pattern across r4/r5/r6**: each round added a
  predicate whose PRODUCTION FAILURE MODE was different
  from its UNIT TEST MODE. The diagnostic ladder
  exhausted not because predicates were wrong but
  because test-discipline didn't surface the gap before
  $20-30/round of stress. **R28-DISCIPLINE codifies the
  discipline that closes the gap.**
- **Trapezoid-of-coverage chain (7th round)**: 9 entry +
  28+ sanitize + 0 exit tests. Under Option C the chain
  carries DIFFERENT failure texts with the SAME shape.
  Ask becomes: Phase 2 stress will be wire-egress oracle
  for a NEW failure class; without R28-T2 the trapezoid
  extends another round.
- **Highest-leverage closure for r29**: **R28-DISCIPLINE**
  — ~80 LOC documentation. Prevents the next 6 Phase 2/3
  predicates from repeating r5-A. Single highest-leverage
  closure since R25-T4's pure-helper extraction landed
  13 tests in one commit.
- **R28-DISCIPLINE landing-gate recommendation**: do NOT
  merge the Phase 2 `stageDiskImages` op without
  R28-DISCIPLINE adopted as a documented rule AND
  R28-PHASE2-TEST-PLAN's 6 cases landed AT MINIMUM.
  Phase 4 default-flip cannot rely on stress validation
  alone if predicates passed unit tests via the same
  failure mode that let r5-A through.
- **No emoji, no celebratory framing**: r28 is
  retrospective-and-forward-looking. Retrospective half
  (R28-DISCIPLINE) is the most important r28 artifact;
  forward-looking half (R28-PHASE2-*) sets up r29 to
  validate Phase 2 with the discipline applied. Net
  direction: structural pivot in flight, test-discipline
  newly named, pre-cutover test budget ~620 LOC across
  ~6 ADR-Phase-2 commits.
