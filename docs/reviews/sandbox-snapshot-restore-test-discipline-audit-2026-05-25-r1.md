# Sandbox/snapshot-restore — R28-DISCIPLINE adoption audit r1

Date: 2026-05-25 (UTC). HEAD at audit: `3a8dce02`
(`sandbox-snapshot-restore` worktree tip; T-8b-stress-r7-A pg
`max_connections` bump landed + r7-C-followup
`start_housekeeper` landed). Cross-worktree state:
`nomad-driver-ch` HEAD carries Option C Phase 2 driver-side
`stageDiskImages` at `b3b1fe59` + tests at `tests/stage_disks_
test.go`. Round 1 of the discipline-adoption sub-track. Parent
rule: `docs/reviews/sandbox-snapshot-restore-test-coverage-
2026-05-25-r28.md` § R28-DISCIPLINE (lines 137-208).

Scope: light, focused. Cite source + test file/line; assess
fixture posture against R28-DISCIPLINE rule; do NOT propose
fixes (deferred-backlog items only).

**Read-only**: NO code edits in this audit. NO git operations.
The in-flight Phase 2 fixture-lag in
`crates/sandbox/src/config.rs` is observed but not touched.

---

## TL;DR

- **r5-A `pollAcquireOFDLock` is the canonical violation** —
  3 dedicated tests (`stop_task_test.go:813-1012`) drive the
  predicate via `SetTryAcquireOFDLockForTest` seam exclusively.
  Zero tests exercise `realTryAcquireOFDLock` (`stop_task.go:
  423-456`). Production EBADF on attempt 1 across 20+ cycles
  was invisible to the 140-test driver suite. Already named
  by R28-DISCIPLINE; this audit documents the witness in the
  current worktree state and adds it as a defunct-by-Option-C
  backlog item (the predicate goes away in Phase 4, but the
  bug class survives — every future driver-side kernel-state
  predicate carries the same exposure).

- **R26-C1 thread-local `Rc<Pool>` has ZERO predicate tests.**
  No `Rc::ptr_eq` test, no "n>1 calls produce 1 connect"
  pin, no 32-thread × 2-role × min_idle production-shape
  fixture. The closure entry in
  `docs/reviews/sandbox-snapshot-restore-deferred.md:1921`
  explicitly acknowledges this with the rationale "real
  `Pool::connect_with_config` is not mockable without a test-
  only seam". r7-A then validated the predicate THROUGH PG
  EXHAUSTION at $2/run × 1 stress cycle — exactly the
  cost-multiplier R28-DISCIPLINE is meant to prevent.

- **T5 `verify_agent_version_post_restore` is COMPLIANT.** 8
  tests at `restore_handler.rs:4182-4384` exercise the
  predicate via a real `spawn_fake_agent` HTTP server. The
  transport-error fixture (`:4322-4340`) constructs production
  state with `TcpListener::bind` + `drop` (ECONNREFUSED on the
  next connect attempt) — that is the real syscall outcome,
  not a happy-path simulation. Gap: only ONE flake shape
  (port-closed → ECONNREFUSED) is covered; production transport
  flakes include connection-reset-mid-body, partial-body, TCP
  read timeout, TLS handshake failure — none of those have
  fixtures. Severity MINOR (the Skipped branch is failure-mode
  forgiving by design — any uncovered flake routes through the
  same WARN+continue path).

- **r4-A `waitForReap` is COMPLIANT (by predicate substitution,
  not direct production state).** The reap predicate
  is `h.exitDone` (channel closed by `superviseCH` after
  `runner.Wait()` returns). Tests at `stop_task_test.go:
  638-780` drive the predicate via a fake runner whose
  `waitCh` is the test-author-controlled `chan struct{}` —
  this REPLACES the real kernel reap (`wait4()` via
  `os/exec.Wait`). The fixture reproduces the OBSERVED state
  the predicate cares about (channel state) but does NOT
  reproduce the kernel state (process zombie → reaped
  transition). This is a weaker version of the same R28-
  DISCIPLINE failure mode r5-A surfaced — but the predicate
  in r4-A reads only the channel, not any kernel-state syscall,
  so the gap is benign at the predicate layer. Real-kernel-reap
  validation comes from the wake-machine integration loop
  (stress cluster). Documented gap, not a defect.

- **Phase 2 driver `stageDiskImages` is PARTIALLY COMPLIANT.**
  Tests at `tests/stage_disks_test.go:39-311` use
  `SetStageImageOpForTest` to substitute a 1-byte-write stub
  (`:51-59`). The downstream predicate that observes kernel
  state is `preflightDiskPaths` (`start_task.go:695-715`) —
  predicate is "file exists + not-a-dir + size > 0", which
  the 1-byte stub satisfies. The ext4-magic post-condition
  R28-T3 calls for (`0x53ef` at byte 1080, the load-bearing
  STRONGER predicate the driver SHOULD enforce) is NOT
  observed by the current predicate AND NOT exercised by the
  current tests. The 1-byte stub would PASS a real `mkfs.ext4`
  failure mode where the kernel created the file but the
  filesystem write failed mid-stream. Backlog item per R28-T3
  + R28-PHASE2-TEST-PLAN.

---

## Audit findings (5 predicates)

### 1. Phase 2 driver `stageDiskImages` — partial compliance

**Source:**
- Predicate emitter: `nomad-driver-ch/ch/stage_disks.go:61-
  133` (`stageImageOpDefault`) + `:186-219` (`stageDiskImages`)
- Downstream observer: `nomad-driver-ch/ch/start_task.go:695-
  715` (`preflightDiskPaths`)
- Tests: `nomad-driver-ch/tests/stage_disks_test.go:1-311`

**Predicate as written:**
`preflightDiskPaths` observes `os.Stat(d.Path)` → file exists,
not-a-dir, `info.Size() > 0`. The Phase 2 contract per
`docs/decisions/2026-05-25-staging-locality.md:229-240` is
"disk images are sparse + mkfs.ext4 + fsync'd before CH spawn".

**Fixture posture:**
- Production state the predicate observes: file freshly
  closed by `mkfs.ext4 -q -F` + truncate + fsync; ext4 magic
  written at byte 1080; superblock journal initialised.
- Test fixture (`:51-59`): `SetStageImageOpForTest` writes 1
  byte (`os.WriteFile(path, []byte("x"), 0o600)`). Satisfies
  `Size() > 0` but is NOT a valid ext4 filesystem — CH on
  spawn would reject this with `vmm: Error opening disk
  image: invalid block device`.

**R28-DISCIPLINE assessment:**

The current predicate (`size > 0`) is satisfied by the test
fixture, so by the letter of the rule the tests reproduce
the production state the PREDICATE observes. The gap is the
STRONGER predicate that should exist (R28-T3 / R28-PHASE2-
TEST-PLAN case 1: ext4 magic at byte 1080). The current
`preflightDiskPaths` is the SAME predicate the controller had
pre-Phase-2 (`stage_disk_image_preconditions`); both miss the
ext4-magic post-condition.

**Verdict:** The current weak-predicate is compliantly tested
against a weak-predicate fixture. Strengthening the predicate
to enforce ext4 magic + idempotency byte-preservation would
expose a new failure mode that needs a production-state
fixture (real `mkfs.ext4` invocation, or a hand-rolled
superblock byte sequence). Tracked: R28-T3 + R28-PHASE2-TEST-
PLAN cases 1 + 5.

**Failure modes NOT currently caught:**

1. Truncate succeeds, `mkfs.ext4` fails mid-stream (kernel
   leaves a truncated file with garbage bytes). The stub's
   1-byte writeFile masks this entirely.
2. `mkfs.ext4 -q -F` succeeds, but `fsync` on the parent dir
   loses the dirent on a power-cut window (T-8b-stress Bug 1).
   The current fsync ordering is unverified by any test.
3. Idempotent re-stage with non-zero existing bytes (operator
   restart mid-stream). `:62-83` ("zero-byte half-staged
   detection") is enforced by code but the test pre-stamps
   1 byte at the path, NOT a partially-mkfs'd file with the
   half-written ext4 superblock that's the actual production
   half-staged state.

**Severity:** IMPORTANT — Phase 2 default-flip (Phase 4) is
gated on these tests existing. Without them, the next stress
round that exposes a `mkfs.ext4` ENOSPC mid-stream will be the
oracle. Per the R28-DISCIPLINE cost analysis: $300+ human-
hours-equivalent vs ~150 LOC test cost.

---

### 2. r4-A `waitForReap` — compliant by predicate substitution

**Source:**
- Predicate: `nomad-driver-ch/ch/stop_task.go:295-341`
  (`waitForReap`)
- Production reap signal: `superviseCH` closes `h.exitDone`
  after `runner.Wait()` returns (= `wait4()` syscall reaped
  the zombie)
- Tests: `nomad-driver-ch/tests/stop_task_test.go:638-780`
  (3 tests: `TestDestroyTask_WaitsForProcessReap`,
  `TestDestroyTask_TolerantOfReapTimeout`,
  `TestDestroyTask_NoOpWhenAlreadyReaped`)

**Predicate as written:**

`waitForReap` polls `h.exitDone` (closed by `superviseCH`)
with 25 × 200ms budget. The predicate observes a CHANNEL
state, not a kernel-state directly — `cmd.Wait` reaps the
zombie internally, then closes the channel. The predicate's
observable is the channel; the kernel reap is a SIDE-EFFECT.

**Fixture posture:**

- Production state: `wait4()` reaps zombie → kernel removes
  PID → fcntl locks held by that PID release.
- Test fixture: `fakeRunner` with test-author-controlled
  `waitCh` (`stop_task_test.go:676` — `f.closeRunner()` →
  the supervisor goroutine observes `Wait()` returning →
  closes `h.exitDone`).

**R28-DISCIPLINE assessment:**

The predicate observes the CHANNEL, and the fixture
constructs CHANNEL state correctly (closed via a fake
runner's Wait returning). At the predicate layer (channel
poll loop), the fixture reproduces the observable state.

The DOWNSTREAM kernel state (fcntl lock release) is NOT
observed by `waitForReap`. r5-A added `pollAcquireOFDLock`
specifically because `waitForReap`'s channel-state predicate
is necessary-but-not-sufficient — r5-A is the surface where
R28-DISCIPLINE was violated, not r4-A.

**Verdict:** COMPLIANT. The predicate observes a channel; the
fixture closes the channel correctly. No production state is
unreproduced AT THE PREDICATE LAYER. The gap r5-A surfaced is
a DIFFERENT predicate (kernel `__fput`), not a fixture defect
in r4-A.

**Caveat / latent gap:**

The fake runner's `waitCh` close is not 1:1 with `wait4()`
ordering — in production the kernel may close `h.exitDone`
within microseconds of the zombie being reaped, OR may take
hundreds of milliseconds if `__fput` is workqueue-backed up.
The test cannot distinguish "channel closed because Wait
returned because reap completed" from "channel closed because
the fake runner's caller closed waitCh." This is acceptable
because the predicate doesn't claim a kernel guarantee — it
claims "the supervisor saw Wait return."

**Severity:** None — documented for completeness; no backlog
item needed.

---

### 3. r5-A `pollAcquireOFDLock` — KNOWN VIOLATION, already named

**Source:**
- Predicate: `nomad-driver-ch/ch/stop_task.go:469-495`
  (`pollAcquireOFDLock`)
- Real syscall path: `:423-456` (`realTryAcquireOFDLock`) —
  `unix.Open` + `unix.FcntlFlock(F_OFD_SETLK, F_WRLCK)`
- Tests: `nomad-driver-ch/tests/stop_task_test.go:813-1012`
  (4 tests: `TestDestroyTask_WaitsForOFDLockRelease`,
  `TestDestroyTask_ProceedsOnLockHeldBudgetExhausted`,
  `TestDestroyTask_NoOpWhenLockImmediatelyAcquired`,
  `TestDestroyTask_FileGoneTolerantWhenProbing`)

**Predicate as written:**

`pollAcquireOFDLock` polls `tryAcquireOFDLockFn(path)`
(production: `realTryAcquireOFDLock`) up to 25 attempts ×
200ms. The PREDICATE observes the kernel's `F_OFD_SETLK`
response — `Acquired` / `Busy(EAGAIN/EACCES)` / `FileGone
(ENOENT)` / `Error(other)`. The kernel state observed is
"does any other `struct file` hold an OFD write lock on this
inode" — answered by attempting to acquire the lock ourselves.

**Fixture posture:**

- Production state the predicate observes: post-CH-exit,
  deferred `__fput` pending in workqueue, OFD write lock
  attributed to PID=-1 (the dead task).
- Test fixture (4 tests, all of them): `SetTryAcquireOFDLock
  ForTest(func(string) (ofdLockProbeResult, error) { ... })`
  returns canned results (`Busy` × N then `Acquired`, etc.).
  Production state of the file is NEVER constructed.

**R28-DISCIPLINE assessment:**

**CANONICAL VIOLATION.** The seam test validates `pollAcquire
OFDLock`'s loop structure (count of probes, sleep cadence,
counter bumps on budget-exhaust). It NEVER validates
`realTryAcquireOFDLock`. Production EBADF on attempt 1 (every
call, across 60 cycles per `docs/reviews/sandbox-snapshot-
restore-cluster-2026-05-25-T8b-stress-r6.md:103`) was invisible
to all 4 dedicated tests.

The probe path was structurally inert in production —
`destroy_task_lock_held_total` climbed monotonically `16 →
17 → 18 → 19 → 20` because EBADF was triggering the budget-
exhaust branch every time, and the seam test confirmed the
counter bumps on budget-exhaust without ever validating that
production hit EAGAIN-then-acquire.

R28-DISCIPLINE was named partially in response to this
specific failure. The corrective shape is R28-T8's
`TestRealTryAcquireOFDLock_PostSubprocessExit` (~40 LOC) at
`docs/reviews/sandbox-snapshot-restore-test-coverage-2026-05-
25-r28.md:374-396` — subprocess opens + flocks + exits-without-
close, then `realTryAcquireOFDLock` is invoked against the
file with the kernel in deferred-`__fput` state.

**Current status of the predicate:**

Option C Phase 4 deletes the predicate entirely. Per
`docs/decisions/2026-05-25-staging-locality.md:229-295`,
each VM gets its own per-alloc disk paths, so cross-alloc
OFD-lock retention is structurally impossible. The r5-A
probe becomes dead code.

**Severity:** Already-IMPORTANT per R28-DISCIPLINE. Backlog
item below.

**Backlog item [r1-DISC-1]:** Even though Phase 4 deletes
the predicate, the test-discipline failure mode persists for
the next driver-side kernel-state predicate (Phase 2's
`fsync_dir` ordering, any future namespace-unshare validation).
Add R28-T8 as a defunct-by-Option-C-Phase-4 backlog item
documenting the witness, NOT to require the test
retroactively. Rationale: writing the subprocess-fork helper
NOW (against the about-to-be-deleted predicate) wastes ~40 LOC
that gets ripped. Capture the pattern in the discipline ADR
instead.

---

### 4. T5 `verify_agent_version_post_restore` — compliant, one gap

**Source:**
- Predicate: `crates/sandbox/src/restore_handler.rs:3135-3273`
- Tests: `crates/sandbox/src/restore_handler.rs:4182-4384` (8
  tests in `real_backend_tests` module)
- Call site: `crates/sandbox/src/wake_machine.rs:499-510`

**Predicate as written:**

Probes the just-restored agent's signed `/version` endpoint;
compares reported `git_commit` against `CONTROLLER_GIT_COMMIT`.
Returns `Match` / `Mismatch{expected,got}` / `Skipped{reason}`
across 6 distinct skip reasons:
`controller_build_sha_unknown`, `agent_build_sha_unknown`,
`agent_git_commit_missing`, `transport_error`, `non_200_
response`, `body_not_json`.

**Fixture posture:**

All 8 tests use `spawn_fake_agent` — a REAL HTTP server
bound to `127.0.0.1:0` that runs the signature-verification
pipeline + serves the test-author-supplied response. This
constructs production-state HTTP wire traffic, not happy-path
simulation.

The transport-error test (`:4322-4340`) constructs ECONNREFUSED
production state correctly:

```rust
let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
let addr = listener.local_addr().unwrap();
drop(listener);  // port closed; next connect → ECONNREFUSED
```

The `drop(listener)` BEFORE the predicate runs is the real
kernel state — the port is unbindable, `connect()` returns
ECONNREFUSED, the production code's transport-error branch
fires.

**R28-DISCIPLINE assessment:**

**COMPLIANT.** All 8 fixtures construct the production state
the predicate observes:
- Match / Mismatch: real HTTP 200 with realistic JSON
- non-200: real HTTP 503
- body-not-json: real HTTP 200 with non-JSON body
- transport-error: real ECONNREFUSED via bind-then-drop
- agent_git_commit_missing: real JSON sans the field
- agent_build_sha_unknown: real JSON with `"unknown"`
  sentinel
- controller_build_sha_unknown: const arg `"unknown"` (the
  controller-side sentinel — there's no fixture to construct,
  the sentinel IS the input)

**Gap:**

Only ONE transport-flake shape is covered (port closed →
ECONNREFUSED on connect). Production transport-flake variants
NOT covered:

1. TCP connect succeeds, then peer RSTs mid-handshake
   (firewall race, post-restore agent crash window).
2. TCP connect + handshake succeeds, then peer hangs on
   write (`call` blocks until the 10s timeout in
   `:3173`).
3. TCP connect succeeds, partial body received, then peer
   closes (`r.into_string()` truncated).
4. TLS handshake failure (if `/version` is ever served over
   HTTPS — currently HTTP).

The Skipped-on-transport-error branch is FAILURE-FORGIVING
by design (returns Skipped + WARN, wake proceeds), so any
uncovered flake routes through the same branch in production.
The branch IS validated; the missing fixtures are alternate
PATHS into the same branch. Severity: MINOR.

**Backlog item [r1-DISC-2]:** Optional — add a "hang for 10s
then close" fixture using a TCP listener that accepts then
never writes. ~30 LOC. Validates the timeout branch
explicitly. Not blocking; the bind-then-drop fixture already
exercises the failure-forgiving branch.

---

### 5. R26-C1 thread-local `Rc<Pool>` — NO predicate tests

**Source:**
- Predicate: `crates/sandbox/src/db.rs:62-105` (`thread_local!
  POOL_APP_CELL` + `POOL_AUDIT_CELL`; `cached_pool` +
  `install_pool`)
- Production consumers: `:617-642` (`open_pool` /
  `pool_app`), `:657-672` (`pool_audit`)
- Closure entry:
  `docs/reviews/sandbox-snapshot-restore-deferred.md:1919-1921`
- r7-A diagnostic (the predicate validated via production
  exhaustion):
  `docs/reviews/sandbox-snapshot-restore-deferred.md:1931-1933`

**Predicate as written:**

`cached_pool` returns the cached `Rc<Pool>` if the DSN
matches; otherwise `None`. `install_pool` writes a fresh
pool into the thread-local, or returns the existing one if a
concurrent insert won the race. The PREDICATE: "successive
calls to `pool_app()` on the SAME compio worker thread
return the SAME `Rc<Pool>` (no fresh `Pool::connect_with_
config` per call)."

The kernel state (sort-of) the predicate cares about:
per-thread TLS storage retains the `Rc<Pool>`; the post-await
re-check window in `install_pool` resolves intra-thread
races correctly.

**Fixture posture:**

- Production state: 32 ntex worker threads, each warmed
  thread has a cached `Rc<Pool>` per role (`POOL_APP_CELL`
  + `POOL_AUDIT_CELL`), each pool has `min_idle=2` retained
  conns + housekeeper-spawned goroutine.
- Test fixtures: **NONE.** Grep across the sandbox crate:
  `Rc::ptr_eq|ptr_eq.*pool|same_pool|cache_hit|cached.*same|
  Rc::strong_count|strong_count` → 0 matches.
  Grep for the cache cells themselves outside `db.rs`:
  `POOL_APP_CELL|POOL_AUDIT_CELL` → 0 matches outside the
  declaration site.

**R28-DISCIPLINE assessment:**

**ZERO COVERAGE of the predicate.** The closure entry at
`deferred.md:1921` explicitly acknowledges this with the
rationale:

> "No new tests added: the cache shape is internal to
> `Database`, observable only via runtime conn-count
> metrics; an 'n>1 calls produce 1 connect' pin test would
> require a connect-counting fixture (real `Pool::connect_
> with_config` is not mockable without a test-only seam)
> and would only re-prove the documented `thread_local`
> shape; the architectural correctness comes from the
> `!Send` invariant the compio-postgres crate enforces at
> the type level."

The argument: "the type system proves it." This is true for
the SAFETY invariant (`Rc<Pool>` cannot leak across threads).
It is NOT true for the BEHAVIORAL invariant (the cache IS
hit on second call; the cached pool retains conns under
load).

**The r7-A diagnostic IS the production-state validation that
should have been a unit test.** Per `deferred.md:1933`: the
R26-C1 retention semantics ran into pg's `max_connections=100`
default, every Nomad worker burned its $2/run hardware cycle,
the diagnostic was a 8-hour read-the-diff investigation, and
the resolution was a config bump + a follow-up `start_house
keeper()` call. EXACTLY the cost the R28-DISCIPLINE rule's
cost analysis (`r28 review:193-202`) predicted.

**Production-state-reproducing fixture that would have caught
r7-A:**

```rust
#[test]
fn r26_c1_min_idle_retention_holds_n_connections_per_thread() {
    // Construct a Database with config pointing at a test pg.
    // Call pool_app() once; observe pg's pg_stat_activity for
    // the role; assert N>=min_idle conns are held.
    // Drop the Database; assert conns drain.
    // Spawn N threads; each calls pool_app() once; assert
    // N*min_idle conns total.
    // ...
}
```

This requires a real pg fixture (already exists at
`crates/sandbox/tests/sandbox_pg_e2e.rs`). The cost is ~80
LOC. The avoided cost was the r7 stress run + r7-A diagnostic
+ r7-C-followup `start_housekeeper` patch.

**Severity:** IMPORTANT — even though r7-A + r7-C-followup
closed the OBSERVED retention issue, the predicate remains
untested. The next change to the cache semantics (e.g., a
DSN-rotation race fix, or a per-role `pool_max` differentiation)
has no test oracle. ~80 LOC backlog.

**Backlog item [r1-DISC-3]:** Add pg-gated test
`r26_c1_min_idle_retention_per_thread_pool` to
`crates/sandbox/tests/sandbox_pg_e2e.rs`. Spawns N threads,
each calls `pool_app()` once, queries `pg_stat_activity` for
role connection count, asserts `N * min_idle <= count <= N *
pool_max`. Validates BOTH the cache predicate (cached pool
returned across calls — measurable via no NEW connections in
pg_stat_activity for repeated `pool_app()` on the same
thread) AND the retention floor that r7-A diagnosed. ~80 LOC.

---

## Summary table

| Predicate | Source | Test posture | R28-DISCIPLINE verdict |
| --- | --- | --- | --- |
| Phase 2 `stageDiskImages` | `stage_disks.go:186-219` + downstream `start_task.go:695-715` | Stub writes 1 byte; satisfies weak predicate (size>0); does NOT exercise mkfs.ext4/fsync ordering | Partial — weak predicate ↔ weak fixture is consistent, but stronger predicate (R28-T3 ext4 magic) needs a stronger fixture |
| r4-A `waitForReap` | `stop_task.go:295-341` | Fake runner closes `waitCh`; channel state reproduced; kernel reap NOT reproduced at predicate layer | Compliant — predicate observes channel, not kernel; kernel-state gap is downstream (r5-A) |
| r5-A `pollAcquireOFDLock` | `stop_task.go:469-495` + `realTryAcquireOFDLock:423-456` | 4 tests use `SetTryAcquireOFDLockForTest` seam; `realTryAcquireOFDLock` UNTESTED | **CANONICAL VIOLATION** — already named by R28-DISCIPLINE; Option C Phase 4 deletes predicate |
| T5 `verify_agent_version_post_restore` | `restore_handler.rs:3135-3273` | 8 tests use real `spawn_fake_agent` HTTP server; transport-error uses real `bind+drop` ECONNREFUSED | Compliant — one MINOR gap: only ECONNREFUSED transport-flake shape covered, not TCP-reset / hang / partial-body |
| R26-C1 thread-local `Rc<Pool>` | `db.rs:62-105` + `:617-672` | ZERO predicate tests; closure entry acknowledges the gap; r7-A diagnosed via $2 stress cycle + 8h investigation | **VIOLATION (uncategorised)** — pure structural argument from `!Send`; behavioral predicate untested |

---

## Defunct vs live predicates

Two of the five predicates are DEFUNCT-OR-SHRINKING under
Option C Phase 4:

- **r5-A `pollAcquireOFDLock`**: deleted in Phase 4 per
  staging-locality ADR. Per-alloc disks → no cross-alloc OFD-
  lock retention surface. The discipline lesson survives the
  predicate's deletion.
- **r4-A `waitForReap`**: still relevant (process-lifecycle
  predicate is orthogonal to disk-image locality), but the
  downstream "what if reap is necessary-but-not-sufficient"
  arm (which r5-A guarded) collapses in Phase 4.

Three predicates are LIVE and will outlast the migration:

- Phase 2 `stageDiskImages` — load-bearing for Phase 4
  default-flip
- T5 `verify_agent_version_post_restore` — controller
  attestation surface; orthogonal to staging
- R26-C1 thread-local `Rc<Pool>` — controller infra; the
  next change here has no test oracle

---

## Backlog items (deferred — not opened in this audit)

For tracking by the parent r28 cycle / Option C Phase 2 sprint:

| ID | Severity | Predicate | Ask | LOC est |
| --- | --- | --- | --- | --- |
| r1-DISC-1 | IMPORTANT (documentary) | r5-A `realTryAcquireOFDLock` | Document the seam-only test pattern as the canonical R28-DISCIPLINE violation in the discipline ADR; DO NOT write the corrective `TestRealTryAcquireOFDLock_PostSubprocessExit` test against an about-to-be-deleted predicate | 0 (docs only) |
| r1-DISC-2 | MINOR | T5 `verify_agent_version_post_restore` transport flakes | Add hang-for-10s-then-close fixture (TCP listener accepts but never writes); validates timeout branch in `:3173-3189` explicitly | ~30 LOC |
| r1-DISC-3 | IMPORTANT | R26-C1 thread-local `Rc<Pool>` retention + cache hit | pg-gated test in `sandbox_pg_e2e.rs`: spawn N threads, each calls `pool_app()`, query `pg_stat_activity` for role conn count, assert `N * min_idle ≤ count ≤ N * pool_max` AND repeated calls on same thread do NOT increment count | ~80 LOC |
| r1-DISC-4 (= R28-T3) | IMPORTANT | Phase 2 `stageDiskImages` ext4 magic post-condition | Driver-side mirror: file exists + non-zero + ext4 magic `0x53ef` at offset 1080. Production-state-reproducing fixture: invoke real `mkfs.ext4` in a sandboxed temp dir (root not required for `-F` on a regular file) | ~80 LOC |
| r1-DISC-5 (= R28-PHASE2-TEST-PLAN cases 1+5) | IMPORTANT | Phase 2 `stageDiskImages` byte-level post-conditions + idempotency | Happy-path × N disks → assert ext4 magic + size; idempotent re-stage with sentinel bytes pre-stamped → assert sentinel survives mkfs skip | ~60 LOC (overlap with r1-DISC-4) |

Total backlog: ~170 LOC of new tests across 5 items; one is
docs-only.

---

## Observations on the r28 discipline rollout

1. **r28 named the rule on 2026-05-25**; this audit
   (`r1`) ran on 2026-05-25 immediately after. Of the 5
   predicates examined, 2 have clean compliance (r4-A by
   structural argument; T5 by production-state HTTP server).
   3 have gaps: 1 canonical violation (r5-A — already
   defunct by Option C), 1 structural-argument-only
   violation (R26-C1 — live and recurring risk), 1 weak-
   predicate-weak-fixture-but-strengthen-needed (Phase 2
   `stageDiskImages`).

2. **The R26-C1 case is the highest-leverage backlog item.**
   It's the only LIVE predicate with ZERO tests AND a
   recent production-cost witness (r7 stress wedge → r7-A
   pg-config bump → r7-C-followup `start_housekeeper`
   patch). The closure entry's "type system proves it"
   argument is correct for safety but blind for behavior.
   The next change to the cache (DSN-rotation, per-role
   `pool_max` split, eviction policy) has no oracle.

3. **The Phase 2 `stageDiskImages` case is interesting**:
   the predicate AS WRITTEN (`size > 0`) is trivially
   testable with a 1-byte stub, and that's what the tests
   do. R28-T3's ask is to STRENGTHEN the predicate to ext4-
   magic at byte 1080, which requires a stronger fixture.
   This is the rare case where a discipline gap is BLOCKED
   on a predicate change, not a fixture change.

4. **T5's compliance is encouraging** — it suggests the
   discipline is naturally adopted when the predicate
   observes a wire protocol (HTTP, signed auth) rather than
   a kernel-state syscall. The seam temptation is weaker
   when the observation surface is already mock-able with a
   real binding (HTTP server on a local port). Kernel-state
   predicates need explicit discipline because the seam is
   the only mockable shape.

5. **r4-A's "compliant by predicate substitution"** is a
   subtle case worth naming: when the predicate is defined
   to observe a SECONDARY signal (a channel close) that is
   downstream of a primary state (kernel reap), the
   discipline applies to the secondary signal, not the
   primary. Tests that close the channel correctly are
   compliant even though they don't reap a real zombie.
   This is the discipline's "letter vs spirit" boundary —
   r4-A is on the letter side; r5-A added a NEW predicate
   that observed the primary kernel state, and that's
   where R28-DISCIPLINE applies.

---

## What this audit does NOT do

- Does NOT propose corrective fixes — backlog items are
  noted for parent-cycle prioritisation
- Does NOT touch in-flight files (the Phase 2 fixture-lag
  in `crates/sandbox/src/config.rs` is observed but not
  patched; the cross-worktree `nomad-driver-ch` files are
  read-only)
- Does NOT re-validate the r28 rule itself; it audits adoption
  of an existing rule
- Does NOT include exhaustive search for OTHER kernel-state
  predicates beyond the 5 named in the brief (a wider sweep
  would be a separate audit pass — candidates: `setup_tap_
  for_vm` EEXIST/idempotency, `materializeRootfs` cp+fsync,
  `taskDiskPathsForLockProbe` enumeration logic, `superviseCH`
  exit detection)

---

## Header / metadata

- Audit type: discipline-adoption (light review per brief)
- Source rule: `docs/reviews/sandbox-snapshot-restore-test-
  coverage-2026-05-25-r28.md` § R28-DISCIPLINE
- Predicates audited: 5 (per brief scope)
- Compliant: 2 (r4-A, T5)
- Violations: 3 (r5-A canonical-defunct; R26-C1 live; Phase 2
  partial)
- Backlog items opened: 5 (1 docs-only; 4 net-new tests;
  ~170 LOC total)
- HEAD at audit: `3a8dce02` (`sandbox-snapshot-restore`),
  Phase 2 driver code at `nomad-driver-ch` worktree
  `b3b1fe59`
- Date: 2026-05-25 (UTC)
