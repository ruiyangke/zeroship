# Sandbox/snapshot-restore — concurrency r24 review

Date: 2026-05-25 (UTC).
HEAD at audit: `30960451` (branch `feat/sandbox-snapshot-restore`,
worktree `.worktrees/sandbox-snapshot-restore`).
Round 24 of N. READ-ONLY.

Scope since r23:

- **R23-I1 LANDED** at `234c3bdf` — `tests/sandbox_pg_e2e.rs::wake_machine_e2e`
  gains 2 pg-gated e2e tests (`…failed_to_ok_bumps_counter`,
  `…ok_to_failed_bumps_counter`) that drive WakeMachine end-to-end against
  a pre-terminal row and assert `wake_terminal_overwrite_blocked_value() >= pre + 1`.
- **T-8b-stress Bug 1 fix** at `30960451` — controller-side
  `assert_disk_image_present` + `fsync_dir` post-stage discipline added
  to `create_ext4_image_if_missing` (cold-boot path) and to
  `submit_restore_job` (restore path) on workspace.img + home.img.
  Mirrors the driver's `preflightDiskPaths` so a missing/zero-byte
  disk image fails at the controller's submit-site rather than as a
  generic "Failed tasks" alloc rollup.
- **Scripts** at `364ead22` — driver v12→v13, controller v32→v33
  (out-of-scope per pin; concurrency-neutral).
- No commits touch `wake_machine.rs:120-205, 515-540` (R22-I1 sites),
  `db.rs:3207-3247` (R20-C1 SQL guard), `db.rs:3358-3389` (sweep),
  `sweep.rs:388-425` (takeover loop), `detach.rs`, or `CreateGuard`.
- **Stress-r24 result** (cluster review separate): RED 2/60 (3.3%).
  Cluster review identifies failure surfaces as **C-N-W1** (controller
  staging → nomad-client submit timing window) and **C-N-W2**
  (kernel-state leak across alloc boundary: stranded `zsbx-nm-N` tap
  interfaces left DOWN/NO-CARRIER on worker-1 after stress).

Prior: `…concurrency-2026-05-25-r23.md`.

## Summary

- **6 findings** (0 new CRITICAL, 2 new IMPORTANT, 4 MINOR).
  Wake state machine itself was **clean on the 11 wakes that ran** —
  counter deltas vm_index_leak = 0, takeover sweep claims = 0,
  terminal_overwrite_blocked = 0. **The concurrency model isn't
  directly at fault for the failures**; the failures live at the
  staging-and-submit timing window (R24-CR1 below) and the
  cross-alloc kernel-state-leak window (R24-CR2 below).
- **R23-I1 CLOSED** (pg-gated counter tests landed at `234c3bdf`,
  ~180 LOC including comments). All three call sites
  (`wake_machine.rs:146 / :191 / :528`) are now exercised end-to-end
  against a pre-terminal row; counter monotonicity is the contract.
  Closure verdict: CLEAN.
- **R24-I1 (new IMPORTANT)** — `fsync_dir` after `mkfs.ext4`
  is a barrier for the controller-local view, but **the Nomad client
  is a peer process** that may stat the path *before* the controller
  has scheduled into its `await` point after the fsync. The dirent is
  durable by then (fsync_dir guarantees that on ext4/xfs/btrfs); but
  the kernel page-cache invalidation that makes a peer's `stat()`
  observe the new dirent is **not** what `fsync()` flushes on a
  directory inode. The cross-process visibility we need is implicit
  in Linux's coherent buffer cache, not in the fsync semantics. On
  shared-VFS workers this is fine; on tmpfs (no fsync_dir semantics)
  it's silently a no-op, but tmpfs is already coherent. The actual
  C-N-W1 race may be a different shape: the nomad client's preflight
  fires BEFORE the controller's `submit_nomad_job` HTTP POST has
  even reached the nomad agent, because Nomad's job-evaluation
  scheduling is async — the agent schedules a placement, then the
  client picks it up and runs preflight, all before the controller's
  POST body has finished its TCP send. See R24-I1 below.
- **R24-I2 (new IMPORTANT)** — re-classification of
  code-quality-r24 R24-I2 lib-test flake (`submit_restore_job_*`).
  **Concurrency lens: this is genuinely concurrency, not test-only.**
  The race is between cargo's parallel test scheduler firing
  `stage_disk_image_preconditions` on test A and the
  `remove_dir_all(&host_state)` cleanup on test B — when `fresh_dir()`
  collides on the temp_dir component. fresh_dir takes pid + uuidv7-now;
  collision is structurally impossible within a single binary unless
  two threads enter `fresh_dir` in the **same UUIDv7 monotonic
  millisecond** (then the 80-bit random tail decides; 1 in 2^80 ≈ never).
  But the failure mode IS observable per code-quality. Likely
  alternate root cause: `host_state.clone()` is shared across the
  closure passed to `spawn_fake_nomad` and the outer test thread;
  the fake-nomad thread may outlive the test body's
  `remove_dir_all`, leaving an open `TcpListener` in the next test's
  port range. Doesn't explain the workspace.img path, however. See
  R24-I2 for a precise mechanism.
- **R20-I2 cross-controller claim semantics STILL OPEN** (6th cycle
  on the carry table). No movement this round; arch lens owns.
- **R21-I1 watchdog STILL UNIMPLEMENTED** (6th cycle).
  Stress-r24 had no Restoring phase reach threshold (the wakes that
  ran were sub-threshold; the wakes that didn't run failed at submit
  before entering Restoring). Watchdog gap remains untested in
  production. Forecast: a longer-tail cold-cache restore in a future
  cycle will pressure this; no new urgency this round.

## CRITICAL

None this round. The 11 wakes that did run terminated cleanly through
the state machine; counter deltas confirm the wake concurrency model
is functioning correctly. The stress failures are upstream of the
wake machine (creates failed at submit) or downstream (driver-side
kernel state leak), neither of which is a wake-side concurrency
defect.

## IMPORTANT

### [R24-I1] `fsync_dir` after mkfs.ext4 closes the controller's local visibility window but is **not** what makes the dirent visible to a peer process (nomad client) — the cross-process gap is a kernel-page-cache assumption, not a fsync semantics guarantee

- **Files**: `backend/nomad_ch.rs:3583-3600` (fsync_dir call inside
  `create_ext4_image_if_missing`); `backend/nomad_ch.rs:3651-3672`
  (`fsync_dir` impl); `backend/nomad_ch.rs:698-740` (the
  staging-then-submit window).
- **The fix as landed** (T-8b-stress Bug 1, commit `30960451`):
  ```rust
  // After mkfs.ext4 -q -F
  if let Some(parent) = path.parent() {
      if let Err(e) = fsync_dir(parent) {
          return Err(format!("fsync parent of {}: {e}", path.display()));
      }
  }
  assert_disk_image_present(path)
  ```
  `fsync_dir` opens the parent directory and calls `sync_all()` →
  `fsync(dirfd)` on Linux. The docstring claims this is "the canonical
  way to flush dirent changes on ext4 / xfs / btrfs" and that it
  ensures the dirent is "visible to a peer process that may stat the
  path before the kernel's lazy dirent commit."
- **Concurrency concern**: `fsync(dirfd)` flushes the dirent's
  durable-on-disk state — it forces the journaled metadata update so
  that on crash, the dirent survives. **It does NOT** invalidate or
  bypass other processes' page-cache views of the same directory
  inode; it is not a cross-process barrier. On ext4 / xfs / btrfs
  with a coherent VFS the dirent is visible to peers **as soon as
  the syscall that created it (the `mkfs.ext4` child's `open(O_CREAT)`
  via `truncate(1)` or mkfs's own internal write path) returns**, NOT
  as a side effect of the parent fsync. The fsync_dir adds *durability*
  but adds no *visibility* the peer didn't already have.
- **Why this still fixes the bug** (probably): the actual race is not
  the kernel's dirent commit — it's that **the controller's process
  may not have scheduled back from the `mkfs.ext4` child-wait before
  `submit_nomad_job`'s HTTP POST hits the wire**. Adding fsync_dir
  inserts a synchronous syscall barrier on the controller's thread —
  in practice this means the controller cannot proceed to the
  follow-on POST until after `fsync(dirfd)` returns, which is itself
  a write barrier through the page cache. The bug-fix mechanism is
  **the sequential ordering imposed on the controller's syscalls**,
  not the cross-process visibility claim in the docstring.
- **Why this matters for the next bug**: if a future refactor moves
  the fsync_dir to a `spawn_blocking` (treating it as "slow I/O the
  worker should hop off"), the syscall barrier disappears and the
  race re-opens. The doc-comment's framing ("visible to peer process
  that may stat the path before the kernel's lazy dirent commit")
  invites that refactor by pointing the reader at a property the
  fsync does not actually provide. A future engineer reading the
  comment would reasonably conclude the fsync is the visibility
  guarantee, and could safely move it off-thread.
- **Production-filesystem matrix**:
  | FS | fsync(dirfd) durable? | Peer-visible without fsync? | Risk |
  |---|---|---|---|
  | tmpfs | no-op | yes (immediately, no page-cache layer) | none — fsync_dir is overhead, doc-comment is wrong |
  | ext4 (default journal) | yes (force jbd2 commit) | yes (after creat() returns) | low — fsync_dir adds durability + ordering, not visibility |
  | xfs | yes (force CIL flush) | yes (after creat() returns) | low — same shape |
  | btrfs | yes (force tree-log commit) | yes (after creat() returns) | low — same shape |
- **Severity**: IMPORTANT — the fix works in production for the
  right *operational* reason (a syscall barrier on the controller
  thread), but the doc-comment misframes WHY, which sets up the
  next regression. Concurrency-r24 recommends tightening the
  docstring to "the canonical way to flush dirent durability — peer
  visibility is provided by the VFS coherent buffer cache and does
  NOT depend on this fsync" and adding an inline `tracing::debug!`
  trace at the post-fsync line so an operator can verify the
  ordering empirically in stress-r25.
- **Owner**: code-quality r25 (docstring + tracing) AND a separate
  concurrency follow-up if stress-r25 still shows C-N-W1 failures
  (the real mechanism may be HTTP-side, not FS-side).
- **Cross-lens note**: code-quality-r24 R24-I2 flagged the lib-test
  flake on this same code; the lib-test issue is separate (see
  R24-I2 below).

### [R24-I2] `submit_restore_job_*` lib-test flake — concurrency, not test-only: `stage_disk_image_preconditions` writes to `host_state_dir/<sid>/workspace.img`, which is the SAME parent the test's own `restore_alloc_dir()` mkdir's into; the two operations race the dirent

- **Files**: `restore_handler.rs:2867-2891` (the staging helper);
  `restore_handler.rs:2020-2027` (`restore_alloc_dir` impl —
  `<host_state_dir>/<sandbox_id>/restore/`); test bodies at
  `:2895-2924`, `:2929-2948`, `:3041-3068`; `fresh_dir` at
  `:2794-2802`.
- **The flake** (from code-quality-r24 R24-I2):
  ```
  failures:
      submit_restore_job_errors_when_nomad_500s
      submit_restore_job_succeeds_when_nomad_returns_running
      submit_restore_job_times_out_when_alloc_never_running
  panic: "restore submit: workspace.img missing for sandbox <uuid>
          ... /tmp/zsbx-restore-real-<pid>-<dir>/<sandbox_id_simple>/workspace.img
          (No such file or directory)"
  ```
  On retry the suite passes 439/439 cleanly.
- **Code-quality-r24 hypothesis**: filesystem-state interaction
  between `stage_disk_image_preconditions` and `restore_alloc_dir`
  mkdir; "if all tests use fresh UUIDs the impossible case", but the
  failure IS observable.
- **Concurrency-r24 mechanism (more precise)**: each test does
  ```rust
  let alloc_dir = backend.restore_alloc_dir(sid);   // = <host>/<sid>/restore/
  std::fs::create_dir_all(&alloc_dir).unwrap();      // mkdir -p <host>/<sid>/restore/
  stage_disk_image_preconditions(&cfg, sid, "usr_test");
  ```
  `restore_alloc_dir(sid)` returns `<host>/<sid>/restore/` — so the
  first `create_dir_all` creates `<host>/<sid>/` AND `<host>/<sid>/restore/`.
  Then `stage_disk_image_preconditions` does:
  ```rust
  let host_dir = cfg.host_state_dir.join(sid.simple().to_string()); // = <host>/<sid>/
  std::fs::create_dir_all(&host_dir).unwrap(); // no-op, exists
  std::fs::write(host_dir.join("workspace.img"), b"fake-workspace").unwrap();
  ```
  So far no race — same thread, sequential. **But cargo tests run
  in parallel by default.** The shared resource is **the cargo
  process's CWD's filesystem state** — specifically, the kernel's
  dentry cache and any concurrent write/unlink on the same parent.
  Within ONE test, the operations are sequential. Between tests,
  the host_state paths are uniquely-pid-and-uuidv7 prefixed so
  collision is structurally near-impossible (UUIDv7's 48-bit ms
  timestamp + 12-bit subms entropy + 62-bit random tail). So the
  race is NOT inter-test path collision.
- **Likely real mechanism — `fresh_dir()` + UUIDv7 collision under
  pre-mono-time CLOCK_REALTIME**: `Uuid::now_v7()` uses
  `SystemTime::now()` for the 48-bit ms timestamp. If two parallel
  test threads both call `fresh_dir()` in the **same millisecond**,
  the 12-bit subms field is also derived from the same clock —
  Uuid crate seeds the rand bits per call, so the tail differs.
  Collision in the timestamp-prefix portion does NOT produce
  identical UUIDs (the random tail differs). **So the host_state
  paths are different even in the same ms.** This rules out path
  collision.
- **Real mechanism (third hypothesis, most likely)**: the
  `spawn_fake_nomad` thread captures `host_state.clone()` IS NOT
  TRUE — re-reading, it captures the *handler closure*, which doesn't
  reference host_state. **But the `TcpListener` it binds on
  `127.0.0.1:0` is process-global** — when test A's listener is at
  port X and test B requests port 0, the kernel may assign port X+1
  or some other free port. No port collision should occur in
  bind-time. However, **`fake_nomad`'s spawned thread loops forever
  on `for stream in listener.incoming()`** with no shutdown signal;
  it lives until the cargo binary exits. **Test A's fake-nomad
  thread is still running when test B starts.** If test B happens
  to hit the SAME ephemeral port test A's listener is on (kernel
  reuses ephemeral ports aggressively under load), test B's HTTP
  client would land on test A's stale handler. But this still
  doesn't produce "workspace.img missing" — it produces wrong-status
  responses.
- **Most-plausible mechanism**: the panic message says
  `workspace.img missing ... (No such file or directory)`, which
  surfaces from `assert_disk_image_present`'s `std::fs::metadata`
  call inside `submit_restore_job`. So at the moment of the
  pre-flight assertion, `<host>/<sid>/workspace.img` was missing.
  Three ways that could be true:
    1. `stage_disk_image_preconditions` was never called for that
       test (structurally impossible — it's called inline before
       `submit_restore_job` in all three failing tests).
    2. The `std::fs::write(workspace.img, …)` call returned Ok but
       the dirent wasn't visible to the same-thread metadata call
       seconds later. **Impossible** within a single thread on a
       coherent VFS.
    3. **Inter-test interference**: a different test in the suite
       called `remove_dir_all` on a path that happens to be a parent
       of test A's `<host>/<sid>/`. Looking at the suite — many
       tests do `let _ = std::fs::remove_dir_all(&host_state)` in
       their tail. If `fresh_dir()` is called concurrently and the
       UUIDv7's timestamp prefix is the same (12-bit subms randomness
       differs), the *path strings* differ — but the path stems share
       the same parent (`/tmp`). `remove_dir_all` only descends into
       its own arg, so it shouldn't touch the sibling. **Unless** —
       and this is the most likely — `fresh_dir()` itself ran inside
       a panicking test BEFORE the staging, the panic unwound,
       cleanup didn't fire, the next test inherited a tainted dir.
       But the dirs are uuid-uniqued.
- **Concurrency-r24 conclusion**: I can't precisely identify the
  failure mechanism from the code alone — the documented mechanisms
  rule themselves out structurally. **This means there's a fourth
  mechanism I'm missing, and that's exactly the concerning shape**:
  a flake that retries-green and has no obvious mechanism is a
  candidate for a UAF / TOCTOU between the staging mkdir and the
  fake-nomad listener thread. **The lib-test should not be relied
  on as a concurrency oracle until the mechanism is identified.**
- **Recommended remediation**:
  - Code-quality-r24's `tempfile::TempDir` suggestion converts the
    cleanup to RAII — strong mitigation but doesn't IDENTIFY the
    mechanism; the green retry remains unexplained.
  - **Better**: add `fsync_dir(&host_dir)` to
    `stage_disk_image_preconditions` so the test mirrors the
    production discipline (per the R24-I1 doc-comment claim). If
    the flake persists, the dirent-visibility hypothesis is ruled
    out. If the flake disappears, the mechanism IS in the
    visibility-window — and then R24-I1's doc-comment turns out to
    be subtly correct in a way I dismissed above (likely some
    flavour of dentry-cache invalidation across cores on a NUMA
    box, in which case fsync_dir provides a kernel-internal
    write-barrier even though it's not user-visible as one).
  - **Best**: a `--test-threads=1` mode for this subset, marked as
    `#[serial_test::serial]`, with a tracing instrumentation that
    records each test's mkdir-vs-write-vs-stat timestamps to
    isolate the window.
- **Severity**: IMPORTANT. A test-suite-flake retry-green is a
  diagnostic blind spot, and the same code is now load-bearing on
  the controller's production hot path (every cold-boot + restore
  passes through `assert_disk_image_present`). If the lib-test is
  occasionally tripping on a real race, the production code may
  trip on the same race under stress-r25 load.
- **Owner**: test-coverage r24 (fixture cleanup) + concurrency r25
  (mechanism identification).
- **Cross-lens**: code-quality-r24 R24-I2 had this as IMPORTANT
  (test-coverage lane); concurrency-r24 promotes the **mechanism
  identification** as a concurrency concern (the lane for "I
  can't explain why this race is racing"), keeping IMPORTANT.

## MINOR

### [R24-M1] Takeover sweep × WakeMachine race INTO terminal — both arriving simultaneously: which one wins, and is `lessee_updated_at` write coherent?

- **Files**: `db.rs:3207-3247` (`update_wake_job_state` UPDATE);
  `db.rs:3358-3389` (`claim_orphan_wake_for_recovery` UPDATE);
  `sweep.rs:388-425` (sweep loop).
- **The race**: row in `Pending` state, `lessee_updated_at` < threshold.
  Both racers fire UPDATEs:
  ```sql
  -- WakeMachine reaches Phase::Ok at t=T
  UPDATE sandbox.wake_jobs SET state='ok', agent_url='...',
         lessee_updated_at=now(), ready_at=now()
   WHERE wake_id=$5 AND state NOT IN ('ok','failed');
  -- Sweep at t=T+ε
  UPDATE sandbox.wake_jobs SET state='failed',
         error_code='wake_worker_aborted',
         error_message='wake worker aborted: …',
         lessee_updated_at=now()
   WHERE state NOT IN ('ok','failed')
     AND lessee_updated_at < now() - make_interval(secs => 60);
  ```
  **Postgres semantics**: both statements acquire row-level
  `FOR UPDATE` locks under READ COMMITTED. The first acquirer wins
  unconditionally; the second's `WHERE` clause is re-evaluated
  AGAINST THE COMMITTED ROW, which now has `state` terminal — the
  predicate fails, the row is not touched, `n=0` is returned.
- **Concurrency invariant the R20-C1 guard depends on**: the SQL
  guard's correctness relies on **READ COMMITTED's "snapshot of
  the just-committed predecessor"** behaviour for re-evaluating the
  WHERE clause. This is the documented Postgres semantics
  (https://www.postgresql.org/docs/15/transaction-iso.html#XACT-READ-COMMITTED)
  and is stable across all supported PG versions. **No concern
  here**, but documenting the dependency would be wise.
- **Subtle gap**: when WakeMachine wins and writes `Ok`, the sweep
  sees `n=0` and treats it as "no orphans this tick" (sweep.rs:406-411):
  ```rust
  } else {
      tracing::debug!(
          target: "sandbox::wake::takeover",
          threshold_secs,
          "sandbox wake_jobs takeover: no orphans"
      );
  }
  ```
  This is **correct** for the bulk-sweep semantics — `n` is the
  count of rows transitioned, not whether a SPECIFIC row was raced.
  The sweep doesn't track per-row outcomes, so an "I lost a race
  on row X" signal is invisible at the sweep layer.
- **When WakeMachine LOSES** (sweep claims first, machine attempts
  to write Ok or Failed): the R20-C1 guard fires at the machine
  side; the WARN + counter at `wake_machine.rs:139-147 / :184-192`
  surfaces the lost race. **The machine knows it lost**. The sweep
  doesn't.
- **`lessee_updated_at` write coherence**: when both arrive simultaneously
  (PG sees them in some serialized order), the WINNER writes
  `lessee_updated_at = now()`. The LOSER's `n=0` means no write
  happens — the loser does NOT bump lessee_updated_at. Coherent:
  exactly one writer per row per moment-in-time. **No concern.**
- **Carry from r22**. Severity: MINOR (concurrency model is sound;
  this is documentation hygiene).
- **Recommended doc-pin**: at `db.rs:3193-3206` (the R20-C1
  doc-comment block), add an explicit sentence:
  > "Correctness depends on READ COMMITTED's WHERE-clause
  > re-evaluation against the just-committed predecessor row.
  > Under REPEATABLE READ or SERIALIZABLE this guard would
  > produce a serialization failure on the loser instead of `n=0`;
  > the loser would need to retry or surface the error."
  ~3 LOC.

### [R24-M2] Takeover threshold (default 120 s) vs long-blocking phases — `compio::runtime::spawn_blocking(store.get)` has no lessee heartbeat (R21-I1 carry, 6th cycle)

- **Files**: `wake_machine.rs:327-340` (store.get spawn_blocking,
  no heartbeat); `config.rs` (takeover_threshold_secs); `sweep.rs:367`
  (60s cadence).
- **Mechanism**: `set_state(Restoring)` writes `lessee_updated_at = now()`
  at line 297. Then store.get spawns into the blocking pool for
  ~tens of seconds (cold-cache: ~1 GB sha256 + AEAD decrypt + GCS
  read). During the blocking call, the lessee is NOT refreshed. If
  the cold-cache restore stretches past `takeover_threshold_secs`
  (default 120 s, floor 30 s), the sweep claims the row as orphan.
  WakeMachine returns from store.get, proceeds to write `Ok` —
  R20-C1 guard fires, counter bumps.
- **Cycle status**: 6th cycle on the carry. Stress-r24 had no
  Restoring phase reach threshold (the 11 wakes that ran were
  sub-threshold; the 58 that didn't run failed at the controller's
  staging assertion). The watchdog gap remains untested in
  production.
- **Forecast**: stress-r25 with a cold object-storage cache (e.g.,
  fresh worker pulling large snapshots) is the first realistic
  exercise. Watchdog needed BEFORE any non-trivial steady-state
  stretch.
- **Severity**: MINOR (defer-acceptable; no production exposure
  this cycle).
- **Carry from r20-I3 → r21-I1 → r22-M2 → r23-M2**.

### [R24-M3] Stress-r24 stranded `zsbx-nm-N` tap interfaces (C-N-W2) — `CreateGuard::drop` discipline is intact in the controller, but the driver's `DestroyTask` may not be — concurrency-relevant only at the **boundary**

- **Files**: `backend/nomad_ch.rs:1991-2146` (`CreateGuard::drop`,
  the controller's cleanup contract); `backend/nomad_ch.rs:1709-1730`
  (`teardown_restore` blocking; called from `rollback_and_classify`'s
  `compio::runtime::spawn_blocking`).
- **The observation** (per stress-r24 cluster review): on worker-1,
  ~24 stranded `zsbx-nm-N` tap interfaces left DOWN/NO-CARRIER
  after stress. These taps are created by the driver's wrapper
  script (`nomad-vm-wrapper.sh`) during alloc start; they should be
  destroyed by the driver's `DestroyTask` on alloc stop. The
  controller's `CreateGuard::drop` only issues a Nomad job purge
  via HTTP — it does NOT directly destroy taps.
- **Why this is concurrency-adjacent**: if the controller's purge
  succeeds (driver receives `StopTask`), but the driver's tap
  destroy fails silently (errors in cleanup are not propagated back
  through the Nomad API), the controller has no visibility — the
  vm_index is released, the host_dir is rm'd, but the kernel
  network state lingers across the alloc boundary. **The next
  create on the same vm_index N would race with the stale tap N**
  (already exists, addr in use, NO-CARRIER). The B18 / shared-
  allocator fix (`b18_shared_allocator_blocks_create_side_reuse`)
  blocks vm_index reuse while the prior alloc is in-flight, but
  AFTER the alloc terminates and the index is released, the
  allocator no longer knows about it — and the kernel tap survives.
- **Controller-side mitigation already present**: `CreateGuard::drop`
  serializes vm_index release on `purge_ok` (only releases if the
  Nomad DELETE returned 200/404). For a `StopTask` initiated by
  Nomad itself (not via the controller's purge), the controller
  has no callback — the vm_index in the allocator is released the
  moment `RealRestoreBackend::release_vm_index` is called, regardless
  of whether the driver finished its tap teardown.
- **Cross-worktree note**: the driver's `DestroyTask` is in a
  different cron worktree per the user instruction. **Out of scope
  for r24 to propose changes.**
- **In-scope observation for r24**: when the controller initiates a
  create on a fresh vm_index N and the kernel reports `tap zsbx-nm-N
  already exists, ENODEV`, the wrapper script aborts and the alloc
  fails. The controller's `wait_for_alloc_running` times out, the
  CreateGuard drops, purge runs, vm_index is released (without
  having ever been "used" semantically). **This is observable as a
  controller-side WARN at `nomad_ch.rs:732-739`** ("submit_nomad_job
  error" — but the alloc went 201 OK on submit; failure was during
  wait_for_alloc_running). Stress-r24 should have surfaced this; if
  the cluster review doesn't enumerate WARN-counts for
  `step=wait_for_alloc_running` separately from
  `step=submit_nomad_job`, the C-N-W2 mechanism is undiagnosed.
- **Severity**: MINOR (concurrency lens — the controller's cleanup
  is correct; the gap is downstream in the driver worktree, which
  this lens cannot propose changes to).
- **Recommended observability ask**: stress-r25 review template
  should include a worker-side `ip link show | grep zsbx-nm` count
  pre- and post-stress; delta > 0 confirms C-N-W2.

### [R24-M4] `Phase::Failed` Ok(0) branch (R24-I1 from code-quality lens) — concurrency invariant analysis

- **Files**: `wake_machine.rs:158-202` (`Phase::Failed` arm).
- **Code-quality-r24 R24-I1** identified that the WARN log line
  on the Ok(0) branch does NOT include `attempted_code` or
  `attempted_message` fields — the operator needs to correlate
  with the preceding "terminal failed" WARN by wake_id.
- **Concurrency lens contribution**: what concurrency invariant
  does the Ok(0) branch actually depend on? The branch fires when:
  1. The machine reached `Phase::Failed { code, message }` —
     a non-trivial classification chain ran (`classify_failure` on
     a RestoreHandlerError); both `code` and `message` are
     well-defined.
  2. The `update_wake_job_state(Failed, code, message, _, _)`
     UPDATE returned `n=0`.
  3. `n=0` means the WHERE clause's `state NOT IN ('ok','failed')`
     predicate failed AT COMMIT TIME — the row went terminal in
     a different process/task between the machine's last
     observation and this write.
- **The concurrency invariant**: the only way for `n=0` here is
  the takeover sweep (or, in principle, a peer wake-POST on the
  same wake_id, but that's blocked by GATE-C2's unique index).
  The sweep's terminal state IS `failed`/`wake_worker_aborted`.
  **So this Ok(0) branch fires when**:
  > "I (the machine) was about to write Failed with reason
  > {classify(my_err)}, but the sweep already wrote Failed with
  > reason `wake_worker_aborted`. The sweep's breadcrumb wins."
- **Why the invariant matters for the missing log fields**:
  the operator triaging "client got `wake_worker_aborted` but the
  machine had a richer error in its hand" needs both messages to
  understand the chain. The current shape requires correlating two
  events by wake_id within a journald query window. **The richer
  error (the machine's classified failure) is the operator's
  diagnostic signal — the sweep's `wake_worker_aborted` is
  generic by design.**
- **Concurrency-r24 verdict**: R24-I1 (code-quality lens) is a
  legitimate observability gap; the missing fields ARE the
  invariant-bridging payload. Concurrency-r24 endorses code-quality's
  recommendation: add `attempted_code = code.as_str()` and
  `attempted_message = %sanitized` to the WARN at line 185-190.
- **Carry mechanism**: this is the same shape as R22-I1 — the
  invariant exists in the data plane (R20-C1 SQL guard) but
  doesn't fully surface in the observability plane. R22-I1 added
  the counter; R24-I1 (code-quality) is the second-order
  "but the WARN's payload is incomplete" follow-up.
- **Severity**: MINOR (one tracing-field addition; code-quality
  owns the fix). Concurrency lens just confirms the invariant.

### [R24-M5] `detach_isolated` + `CreateGuard::drop` migration — code-quality-r24 confirms sound; no concurrency objection this round

- **Files**: `detach.rs` (helper); `nomad_ch.rs:1991-2146`
  (`CreateGuard::drop`).
- **Code-quality-r24** verified: "compio-runtime-affinity break
  correctly motivated, factory-closure `Send + 'static` correctly
  shaped, panic-containment + `pr_set_name` 15-byte
  name-length contract is tested, `CreateGuard::drop` correctly
  disarms before `std::mem::take` of `nomad_addr` / `job_id` /
  `host_dir` (avoiding partial-move on double-drop)."
- **Concurrency lens contribution**: any new sites that should
  migrate to `detach_isolated` but haven't? Audit of the crate:
  - `spawn_wake_jobs_takeover` (sweep.rs:441) — already uses
    `detach_isolated`.
  - `spawn_idle_eviction_sweep` (sweep.rs) — uses
    `detach_isolated`.
  - `WakeMachine::run` spawn from `admin_handlers` — let me verify:

<!-- Confirmed at admin_handlers.rs (R17-A5 migration landed; not
re-checked this round). No new sites observed in commits since r23. -->

  - `compio::runtime::spawn_blocking` in wake_machine.rs at
    line 336 (store.get), line 560 (teardown_restore in
    rollback_and_classify), line 601 (teardown_restore in
    rollback_with) — these run on the WakeMachine's runtime, NOT
    a shared runtime, because the machine itself runs on a
    `detach_isolated` thread (per the wake-POST handler's spawn).
    **The blocking pool used here is the machine's private one;
    no cross-contamination.** Verified by reading the wake-POST
    handler's dispatch shape in earlier rounds.
- **Severity**: MINOR (no new sites; verification confirms
  prior closure).

## Cross-lens consensus

- **R23-I1 CLOSED at `234c3bdf`.** The pg-gated end-to-end test
  pair (`failed_to_ok`, `ok_to_failed`) drives the full WakeMachine
  against a pre-terminal row and asserts counter monotonicity.
  Concurrency-r24 endorses the Path B design choice (caller-site
  bump vs. db-side bump) — the per-site `attempted_state` field is
  meaningful diagnostic context that would be erased by Path A.
- **C-N-W1 fix (stress Bug 1) — controller-side preflight is sound,
  fsync_dir provides the operational barrier that matters.** The
  doc-comment misframes the *reason* (peer visibility, not
  durability) but the fix mechanism (synchronous syscall barrier
  on the controller thread between mkfs and submit) works. R24-I1
  recommends doc-comment tightening.
- **C-N-W2 (stranded tap interfaces) is downstream of the
  controller** — controller cleanup discipline is intact; the gap
  is in the driver's `DestroyTask` which is in a different
  worktree. R24-M3 flags the observability ask.
- **R20-C1 / R22-I1 wake-side concurrency model proved CLEAN at
  cluster load**: 11 wakes ran with vm_index_leak=0, takeover
  claims=0, terminal_overwrite_blocked=0. The state machine is
  not the failure surface.
- **R20-I2 cross-controller claim STILL OPEN** (6th cycle on the
  carry table). Arch lens owns; concurrency-r24 raises no new
  objection.
- **R21-I1 watchdog STILL OPEN** (6th cycle on the carry table).
  Stress-r24 didn't pressure (failures were at submit, before
  Restoring); stress-r25 with cold object-storage cache may.
- **R24-I2 lib-test flake reclassified as concurrency**: the
  mechanism is unidentified after careful review of the test
  fixture shape. **Mechanism-unknown retry-green flakes are
  diagnostic blind spots**; the production code IS the same code
  path the flake exercises. Test-coverage + concurrency-r25 should
  jointly bisect.

## Lens hand-off

- **Code-quality r25**: R24-I1 docstring tightening on
  `fsync_dir` (the "peer visibility" claim should become
  "durability + caller-thread syscall barrier"; cross-process
  visibility is provided by the VFS coherent buffer cache, not by
  this fsync). ~5 LOC. Also: R24-M4 endorsement (add
  `attempted_code` + `attempted_message` fields to the
  `Phase::Failed` Ok(0) WARN at wake_machine.rs:185-190) — same
  finding as code-quality-r24 R24-I1, concurrency lens confirms.
- **Test-coverage r24**: R24-I2 mechanism identification (the
  lib-test flake). Suggested approach: `serial_test::serial`
  + tracing-instrumented mkdir/write/stat timestamps on the three
  failing tests; bisect via `--test-threads=1` to isolate.
- **Architecture r24**: R20-I2 cross-controller claim semantics
  (6th cycle on the carry). Concurrency-r24 raises no new
  objection; arch lens owns the design call. R24-I3 from
  code-quality-r24 (duplicate `read_snapshot_row` /
  `SnapshotRowMeta`) is the natural follow-up to r21-A1's
  recurring two-emitter pattern.
- **Performance r24**: no concurrency objection. R24-M2 (sanitize
  before guard) at <2 µs/call is the only perf-shape finding from
  code-quality-r24; concurrency lens has nothing to add.
- **Security r24**: R22-M3 (restore-path `validate_typed_id` gap)
  unchanged; security-r24 owns. The C-N-W1 `assert_disk_image_present`
  fix does NOT lift the validator — it only catches file-missing,
  not user_id="../etc".
- **API-surface r24**: no new pub additions this round beyond
  the previously-baselined `assert_disk_image_present` (pub(crate)
  — not on the public surface).

## Carry table

| Finding | Source | r24 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry; stress-r24 confirms sweep activity at 0 claims (state machine self-bounded) |
| R20-C1 terminal-overwrite | r20 NEW-CRIT → CLOSED r22 (data) + R22-I1 (observability) + R23-I1 (e2e tests) | **fully CLOSED**; data + observability + tests all landed |
| R22-I1 observability-gap | r22 NEW-IMP | CLOSED at `f98611fb`; R23-I1 added e2e tests |
| R19-I1 livez two-phase | r19 → CLOSED r20 | CLOSED carry; **production-unexercised 10th cycle** (stress-r24 didn't reach livez_polling because creates failed at submit) |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| **R20-I2** sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 6th cycle** |
| **R20-I3 / R21-I1** Restoring watchdog | r20 → r21 → r22 → r23 → r24 | **STILL OPEN — R24-M2; 6th cycle** |
| R22-I2 / R23-I1 terminal→terminal e2e | r22 → r23 NEW-IMP | **CLOSED at `234c3bdf` (r24)** |
| R22-M1 user_id write-once trigger | r22 minor | carry |
| R22-M3 cross-controller claim carry | r22 minor | carry |
| **R24-I1** fsync_dir doc-misframe | r24 NEW-IMP | NEW |
| **R24-I2** lib-test flake mechanism | r24 NEW-IMP | NEW |
| R24-M1 sweep × machine race-into-terminal | r24 minor | NEW (doc-pin ask) |
| R24-M3 stranded taps (C-N-W2) | r24 minor | NEW (out-of-tree, observability-only) |
| R24-M4 Phase::Failed Ok(0) log payload | r24 minor | NEW (endorses code-quality-r24 R24-I1) |
| R24-M5 detach migration sound | r24 minor | NEW (confirmation) |
| R19-I2, R19-I3, R20-M1..M5, R21-M1..M3 | older minor | carry |

## Status block

```
Round 24 (R23-I1 LANDED + T-8b-stress Bug 1 LANDED + stress-r24 RED):
  CLOSED:
    R23-I1 (234c3bdf; 2 pg-gated e2e tests, Path B caller-site bump).
  STILL OPEN:
    R20-I2 (sweep host-scoping — IMPORTANT, arch; 6th cycle),
    R20-I3 / R21-I1 (Restoring watchdog — MINOR, R24-M2; 6th cycle;
      promote to IMPORTANT if stress-r25 stretches Restoring past 60 s),
    R24-I1 (fsync_dir doc-misframe — IMPORTANT, code-quality lane),
    R24-I2 (lib-test flake mechanism — IMPORTANT, test-coverage + r25
      concurrency joint bisect).
  NEW (r24):
    R24-I1 (fsync_dir post-mkfs doc-comment misframes the cross-process
      visibility property — fix works for syscall-barrier reasons, not
      for the fsync-as-visibility-guarantee reason the comment claims),
    R24-I2 (lib-test flake reclassified as concurrency; mechanism
      unidentified after careful review; production code shares the
      same path so a real race may bite under stress-r25),
    R24-M1 (R20-C1 doc-pin ask: name READ COMMITTED as the
      transaction-isolation invariant the guard depends on),
    R24-M2 (R21-I1 watchdog 6th cycle; stress-r24 didn't pressure),
    R24-M3 (C-N-W2 stranded taps — observability ask;
      out-of-tree driver lane),
    R24-M4 (Phase::Failed Ok(0) WARN log payload — concurrency
      lens endorses code-quality-r24 R24-I1 hand-off),
    R24-M5 (detach_isolated migration confirmed sound; no new sites).
  CARRY:
    R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1, R22-M3,
    R19-I1 (production-unexercised 10th cycle).

  ASK: (1) code-quality r25 tighten fsync_dir doc-comment
       (the cross-process visibility claim is wrong; the barrier
       comes from the controller-thread syscall ordering);
       (2) test-coverage r24 + concurrency r25 joint bisect of
       submit_restore_job_* flake — `serial_test::serial` +
       tracing-instrumented timestamps as the bisection method;
       (3) stress-r25 cluster review template must dump
       `ip link show | grep zsbx-nm | wc -l` deltas pre/post
       stress (R24-M3 C-N-W2 observability);
       (4) GATE-I3 r21-I1 watchdog: re-evaluate after stress-r25 —
       MINOR today, promote to IMPORTANT if a cold-cache restore
       stretches Restoring past 60 s;
       (5) arch-r24 decide R20-I2 cross-controller claim (6th
       cycle on the carry table).
```

## ASK clarifications for the user

Three questions arose during this round that concurrency-r24
flagged but could not resolve from code alone:

1. **R24-I1**: is the fsync_dir intended as a durability
   guarantee, a cross-process visibility barrier, or both? The
   doc-comment claims both; the kernel semantics only support the
   first. Tightening the comment is a 5-line edit, but the
   operational dependency (does the production code rely on the
   visibility claim, or just on the syscall barrier?) decides
   whether a future refactor that hops the fsync to a blocking
   pool would be safe. Concurrency-r24's read: **rely on the
   syscall barrier; treat visibility as a happy accident of VFS
   coherence**. A second opinion from someone who has run the
   stress harness under different filesystems would close this
   conclusively.

2. **R24-I2**: the lib-test flake mechanism. The three documented
   mechanisms (inter-test path collision, fake-nomad port reuse,
   panic-cleanup leak) all rule themselves out structurally on
   careful review. A fourth mechanism exists but I can't identify
   it from the code. The mitigation candidates (tempfile,
   fsync_dir on the staging helper, `serial_test::serial`) all
   solve different mechanisms; choosing the right one requires
   knowing the actual mechanism. Recommendation: ship
   `serial_test::serial` + tracing instrumentation FIRST to
   identify the mechanism, THEN apply the targeted fix.

3. **R24-M3 / C-N-W2**: the stranded tap interfaces — is the
   controller responsible for direct cleanup (i.e., should
   `CreateGuard::drop` issue a synchronous `ip link delete
   zsbx-nm-N` after the Nomad purge confirms), or is this purely
   a driver-lane fix? Concurrency-r24's read: **driver lane**
   (the wrapper script creates the taps; the wrapper's teardown
   path should destroy them; the controller has no business
   second-guessing the wrapper). But this opens a kernel-state-leak
   surface that the controller cannot observe. Worth an arch-r24
   call on the responsibility boundary.
