# Sandbox/snapshot-restore — concurrency r27 review

Date: 2026-05-25 (UTC).
HEAD at audit: `054bd9ef` (worktree
`.worktrees/sandbox-snapshot-restore`, branch `master`).
Round 27 of N. READ-ONLY.

Scope since r26 (`d0d7abd6` → `054bd9ef`):

- **Option C Phase 2 capability bundle** (`2226de7a` config flag +
  `6e928a25` controller emission + `bb178538` ADR progress note) —
  the architectural pivot from the 2026-05-25 staging-locality ADR
  lands behind a feature flag (`SANDBOX_DRIVER_STAGES_DISK_IMAGES`,
  default `false`). When flipped, the controller bypasses its
  `spawn_blocking` truncate + mkfs.ext4 block at
  `backend/nomad_ch.rs:777-792` and emits a `zsbx_stage_disks`
  task-meta field + typed `stage_disk_images` ChPlugin Config
  field instructing the Go driver to materialize workspace.img +
  home.img inside `StartTask` before CH spawn.
- **R26-I1 R27-M2 LATENT bytes-as-char Latin-1 cast fix**
  (`821cc9bd`) — code-quality cleanup in `wake_machine.rs` sanitize
  paths; pure-fn over `&str`, no concurrency surface.
- **R26-C1 thread-local Rc<Pool>** (`ee702d5f`) — perf-lens
  optimization replacing pool-per-call with thread-local cached
  pool. Concurrency-r27 reviews this separately at the bottom
  (cross-lens recheck: does the cached pool's
  per-thread-isolation introduce any unsafe state under wake-vs-
  sweeper contention?).
- **R27-I1 BackendBuilder** (`df06d172`) — replaces the
  telescoping `from_config_*` constructors with a typed
  `BackendBuilder`. Identical thread-of-control to r26; the boot
  path still installs `Option<String>` immutably on
  `NomadCHBackend`. No concurrency surface beyond what r26 already
  audited.
- **Stress-r6 RED** (`885e2abb`) — out-of-scope here; stress-r7
  cluster cycle is in flight scripts-only per the round briefing,
  no controller-source overlap.

In flight: stress-r7 cluster sprint (`crates/sandbox/scripts/*`
only; per round briefing, NOT in scope for this lens).

Prior: `…concurrency-2026-05-25-r26.md`.

## Summary

- **6 findings** this round (0 new CRITICAL, 2 new IMPORTANT,
  3 new MINOR, 1 cross-lens DEFER).
- **R26-I2 (Phase::Ok terminal-overwrite divergence) —
  STATUS-CHANGE: priority FALLS** under Option C Phase 2.
  When `driver_stages_disk_images=true`, the failure class that
  motivated R25-I2's divergence (controller staged →
  CreateGuard race → wake/sweep contention on the
  `host_dir`) gets a different actor (driver-side staging).
  R25-I2's reasoning still applies to the LEGACY path (flag
  off, Phase 2 default), but a future Phase 3 cutover obsoletes
  it. The carry mechanism stays OPEN, but the recommended
  disposition becomes "close at Phase 3 cutover, do not invest
  test-coverage R&D NOW." See R27-I-CARRY1.
- **R26-I1 (r3-A boot serial 5s await) — STATUS-CHANGE:
  priority HOLDS** despite Phase 2. Phase 2 does NOT touch the
  boot path; the 5s await + external-dep shape is unchanged.
  But Phase 5 will REMOVE OR demote r3-A Constraints (per the
  ADR Phase 5 disposition table). When Constraints disappear,
  the boot lookup becomes a metric-only feature with no
  jobspec consumer — the failure mode (boot lookup fails →
  None field → "degraded placement") loses its blast radius.
  Recommend keep open for now; reassess at Phase 5.
- **R27-I1 (NEW IMPORTANT): host_dir mtime baseline shifts
  from controller-submit-time to driver-StartTask-time under
  Phase 2**. The sweeper's grace window (`HOST_DIR_GC_GRACE_SECS`,
  default 600s) is computed against the host_dir's mtime. In
  legacy path that mtime is set by the controller's
  `create_dir_all` immediately before `submit_nomad_job`; in
  Phase 2 driver-stages path the host_dir is created by the
  driver INSIDE `StartTask` — typically T+5s after submit (alloc
  placement latency). Practical impact: NEUTRAL for steady
  state (600s grace ≫ 5s offset). LATENT for failed-create
  scenarios where the alloc never reaches StartTask: the
  host_dir is NEVER created on disk, the
  sweeper has nothing to reap, and the "leaked host_dir"
  observability claim at `CreateGuard::drop` (nomad_ch.rs:
  2210-2251) becomes a misnomer (nothing IS leaked because
  nothing was created). The log message at line 2249 may
  surface in operator triage as a false-alarm.
- **R27-I2 (NEW IMPORTANT): home.img per-user race surface
  preserved under Phase 2 but the racing party shifts from
  "two controller tasks" to "two driver allocs"**. Under
  legacy path, the controller's per-task `spawn_blocking` is
  `compio::runtime::spawn_blocking` which serializes through
  the worker's blocking-pool — concurrent CREATEs for the
  SAME user may race on `home.img` creation but the race
  window is small (idempotent `if-missing` check inside the
  closure; mkfs.ext4 is fork+exec which is non-atomic but
  fast). Under Phase 2, two driver allocs on the SAME worker
  (Nomad concurrent placement) race the same idempotent check
  with the same window. The shape is preserved, not closed.
  Flagging because the ADR Phase 5 disposition table makes
  this race the "load-bearing claim" for removing r3-A
  Constraints; if Phase 5 removes Constraints, multi-worker
  same-user concurrent CREATEs land on different workers
  with DIFFERENT host_state_dir paths, and per-user home.img
  becomes per-worker — silently breaking the
  package-cache-persists-across-sandboxes contract. See R27-I2.
- **R27-M1 (NEW MINOR): `CreateGuard::host_dir_created` stays
  FALSE under Phase 2 — the rollback log line is suppressed
  when the driver DID create the dir on a failed alloc**.
  Observability-only; the sweeper still GCs (mtime + DB-state
  gates unchanged); operators looking for the "leaking host_dir"
  log to confirm the sweeper has work to do will not see it on
  Phase 2 failed creates where the alloc reached StartTask.
- **R27-M2 (NEW MINOR): `wait_for_alloc_running` budget
  allocation shifts under Phase 2**. The 120s default
  (`alloc_running_timeout_secs`) now covers Nomad placement +
  driver stageDiskImages + CH spawn vs. legacy "Nomad placement
  + CH spawn." Stage cost is bounded by the legacy ~3-5s
  mkfs.ext4 wall (driver does the same work). Plenty of
  headroom; flag for operator-triage transparency only.
- **R27-M3 (NEW MINOR): R25-I1 sweeper-TOCTOU shape SHIFTS
  but does not close**. The host_dir lifecycle moves from
  controller-owned (CreateGuard) to driver-owned (StartTask)
  under Phase 2, but the sweeper-vs-wake TOCTOU race lives in
  the SWEEPER, not the creator. Whoever creates the dir is
  irrelevant to the sweeper's "row terminal + no pending
  wake_jobs → reap" predicate. R25-I1's open test-coverage ask
  remains the right shape; Phase 2 does NOT obsolete it.
- **R27-DEF1 (NEW DEFER): R26-C1 thread-local Rc<Pool> —
  concurrency-lens recheck CLEAN under wake-vs-sweeper
  contention**. The cached pool is per-thread (Rc, not Arc); a
  hot thread serving wake POSTs uses its own pool reference; a
  detached sweeper task running on `detach_isolated` mints its
  own runtime with its own thread-local pool. No cross-thread
  sharing of `Rc<Pool>`, no double-free, no Send-violation. See
  R27-DEF1.

## CRITICAL

None this round. The Phase 2 capability landing is concurrency-
sound on its happy path: the controller's `spawn_blocking` bypass
is a CONDITIONAL within `try_create`; the unconditional
`submit_nomad_job` + `wait_for_alloc_running` sequence remains
the lock-of-truth for "alloc placed + driver staged + CH
spawned." The driver's `stageDiskImages` runs BEFORE CH spawn
INSIDE StartTask (per the driver-side commit `b3b1fe59` cited
in the ADR's Phase 2 ledger), so there is no race window where
"CH spawns before workspace.img exists" — those two steps are
sequential within the same `StartTask` call on the same worker.

## IMPORTANT

### [R27-I1] (NEW IMPORTANT) host_dir mtime baseline shifts from controller-submit-time to driver-StartTask-time under Phase 2

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:797-821` — the
    Phase 2 if/else split; `if cfg.driver_stages_disk_images`
    skips `create_dir_all` and `create_ext4_image_if_missing`.
  - `crates/sandbox/src/sweep.rs:881-905` — the
    `HOST_DIR_GC_GRACE_SECS` constant (600s default) and its
    framing as "mtime grace."
  - `crates/sandbox/src/sweep.rs:1086-1119` — the sweeper's
    mtime read via `entry.metadata()` + the grace gate inside
    `classify_host_dir_entry`.
  - `crates/sandbox/src/backend/nomad_ch.rs:2210-2251` —
    `CreateGuard::drop` "leaking host_dir" log site.

- **The shape**:

  Legacy path (flag off):
  ```
  T+0:  controller calls create_dir_all(host_dir)
        → host_dir mtime = T+0
  T+0+ε: controller submit_nomad_job (200 OK)
  T+5:  Nomad places alloc → driver StartTask runs
        (mtime unchanged: dir already exists, no touch)
  ```

  Phase 2 path (flag on):
  ```
  T+0:  controller computes workspace_image_path(host_dir)
        DECLARATIVELY (host_dir NOT created on disk yet)
  T+0+ε: controller submit_nomad_job (200 OK)
  T+5:  Nomad places alloc → driver StartTask runs
        → driver calls create_dir_all(host_dir) +
          create_ext4_image_if_missing(workspace.img)
        → host_dir mtime = T+5
  ```

- **Concurrency impact**:

  1. **Sweeper grace window starts T+5, not T+0**. The grace
     gate at sweep.rs:1086-1091 reads
     `metadata.modified()` → `mtime_secs`, then
     `age_secs = now_secs.saturating_sub(mtime_secs)`. For an
     immediate post-create failure, the sweeper sees a 5s
     newer mtime in Phase 2 → eligible 5s later. The 600s
     default grace dominates; the 5s shift is well within
     noise. **NEUTRAL** for steady-state operation.

  2. **Failed-create-before-StartTask path: host_dir never
     created**. Under Phase 2, if the alloc fails to PLACE
     (e.g., Nomad scheduling rejection, node unavailable,
     timeout in `wait_for_alloc_running`), the driver's
     `StartTask` never executes → the host_dir is never
     created → there is NOTHING for the sweeper to reap.
     This is a NEW shape: in legacy path, even a place-
     fails-immediately CREATE leaves a host_dir + workspace.img
     on disk (controller staged before submit); the sweeper
     reaps it after grace. In Phase 2, the same scenario
     leaves NO on-disk state — there is nothing to reap.
     **OUTCOME is BETTER** for steady-state (no leak), but
     the **CreateGuard::drop log line at line 2244-2250
     fires under `host_dir_created` semantics**, which stays
     `false` in Phase 2 → log line is correctly NOT
     emitted. The behaviour is internally consistent. The
     concurrency angle is that a future audit asking "where
     does the failed-place host_dir go?" finds NO trace —
     operators may interpret this as a leak when it is the
     opposite. **MINOR-leaning-IMPORTANT for observability**.

  3. **Failed-create-AFTER-StartTask path: host_dir exists,
     CreateGuard log says nothing**. If the alloc places, the
     driver runs StartTask, stageDiskImages creates the
     host_dir + workspace.img, then SOMETHING fails later
     (CH spawn fails, agent livez timeout, etc.) → the
     controller's `try_create` returns Err → CreateGuard fires
     → cleanup tail at nomad_ch.rs:2244-2250 checks
     `host_dir_created` which is **FALSE** under Phase 2 →
     the "leaking host_dir" log line is SUPPRESSED.

     **The host_dir IS on disk** (driver created it), but
     the log says nothing. The sweeper still reaps it after
     grace via the DB-row terminal gate. **Correctness OK
     (sweeper handles it), observability GAP** (operator
     scanning logs for "leaked dir" sees no event when the
     dir IS on disk).

     This is a divergence between the boolean's NAME
     (`host_dir_created` = "did the controller create it?")
     and what operators expect it to MEAN ("is there a
     host_dir on disk that we need to remember?"). Cross-
     references R27-M1.

- **Practical likelihood**:

  - **Steady-state success**: zero impact. The driver creates
    the dir, the alloc reaches running, the dir lives until
    the sandbox terminates and the sweeper reaps after grace.

  - **Failed-create-place**: the legacy path leaked a small
    dir (just a workspace.img + maybe home.img mkdir);
    Phase 2 leaks nothing. **Phase 2 is strictly better here.**

  - **Failed-create-post-StartTask**: the legacy path's
    CreateGuard logged the leak; Phase 2's CreateGuard
    does NOT. The dir still exists and is still reaped by
    the sweeper — but the log trail is broken. **Phase 2
    is observability-worse here.**

- **Recommendations** (no implementation; reviewer-only):

  1. **Doc-comment** at nomad_ch.rs:797-821 naming the
     semantic shift: `host_dir_created` now means
     "controller created" not "host_dir exists" — operators
     reading the log line at 2244-2250 should treat its
     absence as "controller did not stage, driver may have
     staged" rather than "no host_dir on disk."

  2. **Alternative naming proposal (deferred until Phase 3)**:
     rename `host_dir_created: bool` →
     `host_dir_controller_owned: bool` to make the
     semantics explicit. Phase 3 cutover deletes the
     legacy branch entirely; the bool can be deleted with
     it. Until then, the doc-comment is the cheap fix.

- **Severity**: IMPORTANT (observability gap on a path that
  operators triage with grep-the-logs muscle memory; the
  failure-class semantic is preserved but the log trail
  shifts). Arch-r27 owns the call on whether the doc-comment
  is sufficient or whether to push the rename forward.

- **Cross-lens**:
  - **observability r27**: the missing log line on Phase 2
    post-StartTask failures is a metric-coverage gap. The
    `sandbox_corrupt_id_total` counter (composite-r1) is the
    wrong shape; a `sandbox_host_dir_orphaned_by_driver_total`
    counter would be a clean signal but requires the
    controller to know whether the driver staged — which it
    only knows from the alloc's TaskEvent history.
  - **test-coverage r27**: no unit test asserts the
    `host_dir_created` behaviour under Phase 2. The 4 new
    emission tests at nomad_ch.rs:7438-7570 (per commit
    `6e928a25`) pin the JOBSPEC wire-out fields but not the
    GUARD behaviour. ~20 LOC fixture exercising
    `try_create_with_driver_stage_failure` → assert
    `guard.host_dir_created == false` even though the
    driver may have created the dir.

- **Carry mechanism**: NEW for r27. Arch-r27 owns the design
  call; concurrency-r27 owns the description.

### [R27-I2] (NEW IMPORTANT) home.img per-user race surface preserved under Phase 2; latent multi-worker breakage if Phase 5 removes r3-A Constraints

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:797-821` — Phase 2
    if/else split (controller skips `create_ext4_image_if_missing`
    on home.img when flag set).
  - `crates/sandbox/src/backend/nomad_ch.rs:3939-3960` —
    `create_ext4_image_if_missing` idempotent-check (file exists
    → skip).
  - `crates/sandbox/src/backend/nomad_ch.rs:622-630` —
    `user_home_image_path` derivation (per-user path under
    `user_home_dir_root`, shared across all of a user's
    sandboxes).
  - `docs/decisions/2026-05-25-staging-locality.md:329-339` —
    ADR Phase 5 disposition for r3-A Constraints ("REMOVE OR
    keep as defense-in-depth (decide post-stress)").

- **The shape**:

  home.img is **per-user, shared across all of that user's
  sandboxes** (package caches + dotfiles persist; per the
  doc-comment at backend/nomad_ch.rs:622-630). The path is
  `<user_home_dir_root>/<user_id>/home.img`.

  Legacy path: the CONTROLLER's `spawn_blocking`
  `create_ext4_image_if_missing(home.img)` runs ONCE per CREATE
  for the user. Two concurrent CREATEs for the SAME user race
  the idempotent check inside `spawn_blocking`:

  ```rust
  // pseudocode of create_ext4_image_if_missing
  if !path.exists() {
      truncate -s <size>           // (A) one syscall
      mkfs.ext4 -q -F <path>       // (B) fork+exec, ~1-3s
  }
  ```

  Race window in legacy: between (A) and (B), a concurrent
  task's `path.exists()` returns `true` (file exists with 0
  metadata) → skips both → may observe an unformatted file.
  Mitigation in legacy is `compio::runtime::spawn_blocking`
  serializes-through-pool — but the pool has multiple
  threads. So the race IS possible but narrow.

  Phase 2 path: the DRIVER's `stageDiskImages` (Go-side, in
  `nomad-driver-ch` worktree commit `b3b1fe59`, not visible
  here) does the equivalent if-missing check + truncate +
  mkfs.ext4. Two concurrent allocs for the same user on the
  same worker race the SAME predicate with the SAME window.

  **The shape is preserved, not closed.** Phase 2 moves the
  actor; it does not add cross-actor coordination.

- **Concurrency impact**:

  1. **Same-worker, same-user, concurrent allocs**: the race
     window is identical in legacy and Phase 2. The driver
     uses the same `if-missing` predicate the controller did.
     **NEUTRAL.**

  2. **Cross-worker, same-user, concurrent allocs**: under
     r3-A node-affinity Constraints (still emitted in Phase 2
     per nomad_ch.rs:2647-2648), allocs for a user CAN only
     land on the controller's own node — so cross-worker
     same-user is structurally impossible while
     `local_nomad_node_id` is set. **CONSTRAINED-SAFE under
     Phase 2.**

  3. **Phase 5 disposition is the load-bearing claim**: the
     ADR Phase 5 table proposes to "REMOVE OR keep as
     defense-in-depth (decide post-stress)" the r3-A
     Constraints. **If Constraints are REMOVED**:
     - Alloc-A for user-X places on worker-1 → driver
       stageDiskImages creates `home.img` at
       `worker-1:<user_home_dir_root>/<user_id>/home.img`.
     - Alloc-B for user-X places on worker-2 → driver
       stageDiskImages creates `home.img` at
       `worker-2:<user_home_dir_root>/<user_id>/home.img`.
     - These are on DIFFERENT WORKERS' LOCAL DISKS.
     - User's package caches written in alloc-A are NOT
       visible in alloc-B.
     - **Silently breaks the package-cache-persists contract.**

     This is NOT a Phase 2 bug — Phase 2 keeps Constraints,
     so the per-user home stays single-worker. But the ADR
     Phase 5 dispositions Constraints as "decide
     post-stress," and the decision SHOULD account for the
     home.img-is-per-user-not-per-sandbox invariant.

- **Practical likelihood**:

  - **Phase 2**: zero impact (Constraints active).
  - **Phase 5 with Constraints REMOVED**: 100% breakage
    rate on the second-create-for-a-user lands on a
    different worker. Steady-state, observable as "package
    cache misses after EVERY sandbox restart unless user
    happens to land on the same worker."
  - **Phase 5 with Constraints KEPT as defense-in-depth**:
    zero impact (same as Phase 2). The defense-in-depth
    Constraints become a correctness requirement re-cast as
    a defense-in-depth label.

- **Recommendations**:

  1. **ADR Phase 5 disposition table should be amended**
     to mark r3-A Constraints as **"KEEP for per-user
     home.img persistence"** rather than "REMOVE OR keep
     as defense-in-depth (decide post-stress)." The
     `home.img` per-user invariant is correctness, not
     defense-in-depth, under Phase 2's host_state_dir
     topology.

  2. **Alternative (deferred for Phase 4 cluster validation
     and beyond)**: move home.img from the worker's local
     disk to a shared `TieredSnapshotStore` (the snapshot-
     side `home.img` path already discussed in ADR section
     "Trade-offs" item 2). This would close the per-user
     race entirely AND let Phase 5 remove Constraints
     without breaking persistence. Cost: larger than
     Phase 2's ~230 LOC; not a Phase 2 ask.

  3. **Doc-comment** at backend/nomad_ch.rs:622-630
     naming the per-user-per-worker invariant under
     driver-side staging. Operators reading the
     `user_home_image_path` derivation will not see this
     constraint surfaced unless the docstring carries it.

- **Severity**: IMPORTANT (a latent correctness gap whose
  blast radius depends on the Phase 5 disposition that has
  not yet landed). Arch-r27 owns the Phase 5 decision; the
  concurrency lens raises this BEFORE Phase 4 stress so the
  decision is informed.

- **Cross-lens**:
  - **arch-r27**: the Phase 5 disposition table is the
    decision point. The ADR is "Accepted" but Phase 5 is
    deliberately deferred (see ADR Phase 5 paragraph 1).
    Concurrency-r27 contributes the home.img invariant as
    an input to that deferred decision.
  - **stress-r7 cluster**: the current stress harness runs
    multiple users; if r3-A Constraints are flipped off
    experimentally in stress-r7, the home.img persistence
    failure should be observable as a creator-visible
    "package cache empty on every wake" bug. Easy
    falsification.

- **Carry mechanism**: NEW for r27. Bridges concurrency lens
  → arch lens for the Phase 5 Constraints disposition.

### [R26-I1] (carry — PRIORITY HOLDS) Boot lookup is a serial 5-second `await` adding external dependency

- **Files**: `crates/sandbox/src/lib.rs:670-698`,
  `crates/sandbox/src/backend/nomad_ch.rs:3144-3157`.

- **Phase 2 impact**: ZERO. Phase 2 lands the
  `driver_stages_disk_images` flag + jobspec emission; the
  boot path's `fetch_local_nomad_node_id` await is unchanged
  at the source level. The 5s timeout, the metric bump on
  failure, the WARN log — all unchanged.

- **Forward-looking interaction with Phase 5**: per the ADR
  Phase 5 disposition table, r3-A Constraints become
  "REMOVE OR keep as defense-in-depth." If Phase 5 KEEPS
  Constraints, the boot lookup retains its current role
  (gating placement to the controller's node). If Phase 5
  REMOVES Constraints, the boot lookup becomes metric-only —
  its blast radius collapses (no jobspec consumer, no
  placement-degradation path). Under that disposition,
  R26-I1 demotes itself: the failure mode (lookup fails →
  field stays None → degraded placement) loses its second
  half because there is no longer a "degraded placement"
  shape under driver-side staging anywhere on the same
  worker.

- **R27-I2 dependency**: R27-I2 argues the home.img
  per-user invariant requires keeping Constraints. If
  that argument carries, the boot lookup also stays
  load-bearing, and R26-I1's recommendation set (doc-comment
  → /readyz → background-retry) is the right shape.

- **Status**: STILL OPEN. PRIORITY UNCHANGED. The Phase 2
  landing does NOT touch this surface; reassessment is the
  Phase 5 decision (see R27-I2 cross-reference).

- **Carry to r28** IF Phase 5 dispositions Constraints in
  the next round; close OR reaffirm based on that decision.

### [R25-I2] (carry — PRIORITY FALLS) `Phase::Ok` terminal-overwrite divergence

- **Files**: `crates/sandbox/src/wake_machine.rs:120-156` (the
  `Phase::Ok` terminal-write site) — unchanged since r25
  close.

- **Phase 2 impact**: the failure class R25-I2 describes
  ("Phase::Ok terminal write blocked by sweep → sandbox row
  already `Running` + vm_index held → cluster diverges from
  wake_jobs row") is reachable through the SWEEPER's reap-of-
  host_dir-mid-wake path. Under Phase 2:

  - The host_dir lifecycle MOVES from controller-owned to
    driver-owned (cold-boot CREATE). But R25-I2's racing
    surface is WAKE, not CREATE. Wake-side host_dir
    lifecycle is the snapshot-restore stage, which the ADR
    Phase 2 leaves unchanged ("Cold-boot only — the
    restore branch stages rootfs via the existing
    RootfsSource hardlink/copy"; commit `6e928a25` line
    25-31).

  - So Phase 2 does NOT directly close R25-I2. But the
    Phase 3 cutover ("Controller cutover" — `try_create`
    becomes validation-only) does the same shift for the
    cold-boot branch only; the restore branch is **also**
    moved to driver-side staging in a separate Phase 3
    sub-step (per ADR section "Cons — explicitly named"
    bullet 3 about wake/restore path changes).

  - **Once Phase 3 lands (cold-boot AND wake both
    driver-staged)**, the Phase::Ok terminal-overwrite
    failure class has no remaining producer: the
    CreateGuard rollback path is a no-op (per ADR Phase 3
    description), the sweeper reaps DRIVER-staged dirents
    not in-flight CONTROLLER-staged dirents. R25-I2
    structurally closes.

- **Disposition shift**: from "STILL OPEN — design call
  pending on current-behavior vs atomic two-row CAS" to
  "STILL OPEN — close at Phase 3 cutover, do NOT invest
  test-coverage R&D NOW." The two-row CAS investment would
  be paid off only for the Phase 2 window (cold-boot legacy
  path under flag=false); Phase 3 obsoletes it.

- **Status**: STILL OPEN, PRIORITY FALLS. Recommend hold
  test-coverage ask in the carry table; do NOT promote.

- **Carry to r28**.

## MINOR

### [R27-M1] (NEW MINOR) `CreateGuard::host_dir_created` semantic-overload under Phase 2

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:2073,
  2093, 2104-2256, 4341, 4398, 4446`.

- **Analysis**: see R27-I1 sub-point 3. The boolean's
  name implies "is there a host_dir on disk?" but the
  semantics under Phase 2 are "did the CONTROLLER create
  the host_dir?" — and under Phase 2 driver-stages the
  answer is always False even when the driver did create
  the dir on a failed-post-StartTask alloc.

- **Why MINOR not IMPORTANT**: pure observability gap; no
  correctness deficit (sweeper still reaps; sandbox row
  still terminal-on-failure). The log line at 2244-2250
  becomes a partial signal rather than a full one — but
  the metric trail through `sandbox/nomad-ch create error`
  and the Nomad alloc TaskEvent history both carry the
  full failure record.

- **Recommended doc-comment** at the field declaration
  (line 2073) and the if-guard at line 2244-2250 naming
  the Phase 2 semantic shift. ~5 LOC. Defer to code-quality
  r27 or arch r27 owner-call.

- **Severity**: MINOR (observability hygiene).
- **Carry mechanism**: NEW for r27.

### [R27-M2] (NEW MINOR) `wait_for_alloc_running` budget allocation shifts under Phase 2

- **Files**: `crates/sandbox/src/backend/nomad_ch.rs:867-881`
  (`wait_for_alloc_running` call site),
  `crates/sandbox/src/config.rs:805` (the 120s default), and
  `crates/sandbox/src/backend/nomad_ch.rs:2720-2899` (the
  poll body).

- **The shape**:

  Legacy: `submit_nomad_job → wait_for_alloc_running`. The
  "alloc reaches running" status fires when Nomad's scheduler
  has placed AND the wrapper script (legacy) or driver's
  StartTask (T-8) has begun. Stage-disk-images work (legacy
  controller side) happened BEFORE submit and was outside
  this budget. Typical wall: place latency (~1-3s) + wrapper
  init (~1-2s) = ~3-5s.

  Phase 2: same call sequence. But Nomad's "alloc reaches
  running" status now fires AFTER the driver's
  stageDiskImages completes (the driver runs stage BEFORE
  CH spawn; CH spawn happens BEFORE ClientStatus flips to
  "running"). Stage-disk-images work moves INTO this budget.
  Typical wall: place (~1-3s) + stageDiskImages (~3-5s) + CH
  init (~1-2s) = ~5-10s.

- **Concurrency impact**: NEUTRAL on correctness. The 120s
  default is 10-24× the typical wall under Phase 2; plenty
  of headroom. Tail-latency under disk-pressure or pg-
  backpressure (R26-DEF1 territory) could compress this
  headroom; flagging for operator-triage transparency.

- **Recommendations**: name the budget shift in operator
  docs (`docs/runbooks/sandbox-nomad-ch.md`?) — operators
  watching the `sandbox_create_alloc_running_latency_*`
  histogram in Phase 2 should expect the median to move
  right by ~3-5s.

- **Severity**: MINOR (operator-doc hygiene; no defect).
- **Carry mechanism**: NEW for r27.

### [R27-M3] (NEW MINOR) R25-I1 sweeper-TOCTOU shape SHIFTS but does not close under Phase 2

- **Files**: `crates/sandbox/src/sweep.rs:1014-1223` —
  `run_host_dir_gc_once`, unchanged since r25.

- **The pin task** (per round briefing): "with Phase 2, the
  host_dir lifecycle is now driver-owned (host_dir_created
  stays false on controller). Does the sweeper still need to
  manage host_dir_GC? Or does Option C obsolete the sweeper
  for cold-boot path?"

- **Analysis**:

  1. **Sweeper still required.** The sweeper does NOT care
     who CREATED the dir; it cares whether the dir is
     reapable (terminal sandbox + no pending wake_jobs +
     mtime grace expired). The CREATOR is a documentation
     question, not a behaviour-of-sweeper question. ADR
     Phase 5 disposition table line 335 confirms: "Still
     required for failed-create leak hygiene; reaps
     driver-staged paths | KEEP, rewrite rustdoc to cite
     driver as creator."

  2. **R25-I1's TOCTOU shape preserved**.
     `wake_machine.rs:120-156`'s "wake handler refuses
     terminal sandbox" predicate runs on the WAKE side;
     the sweeper's "reap eligible host_dir" predicate
     runs on the SWEEPER side. The race is between these
     two predicates, not between the sweeper and any
     CREATOR. Phase 2 changes WHO creates but does not
     introduce coordination between wake and sweeper.

  3. **The R25-I1 test-coverage ask remains valid**.
     A pg-gated test pinning "wake handler refuses
     terminal sandbox + zero wake_jobs inserted" + a
     pure-unit test extending the eligibility matrix to
     the terminal triad (Stopped/Lost/Orphan) — both
     remain the right hand-off. Phase 2 does NOT close
     these tests' need.

  4. **What Phase 2 DID change for the sweeper**: the
     mtime baseline (per R27-I1) and the docstring
     accuracy ("controller creates" → "driver creates"
     under Phase 2 cold-boot path). The behavioural
     semantics of the eligibility helpers
     (`classify_host_dir_entry`, `host_dir_eligible_by_db`)
     are unchanged.

- **Severity**: MINOR (confirmation: sweeper retains its
  role; R25-I1 carry mechanism is correct).
- **Carry mechanism**: NEW for r27 (cross-lens
  confirmation against the round briefing question).

## DEFER

### [R27-DEF1] (NEW DEFER) R26-C1 thread-local Rc<Pool> — concurrency-lens recheck CLEAN

- **Files**:
  - `crates/sandbox/src/db.rs` — pool-cache changes per
    `ee702d5f` (thread-local `Rc<Pool>` replacing
    pool-per-call).
  - `crates/sandbox/src/detach.rs:detach_isolated` —
    spawns OS-thread with private compio runtime; detached
    work (CreateGuard cleanup, host-dir-gc, sweeper) runs
    on its own thread with its own pool cache.

- **The concurrency-lens question** (recheck of perf-r26
  C1): "does the per-thread Rc<Pool> cache introduce ANY
  cross-thread safety issue (Send/Sync, double-free, racing
  pool initialization)?"

- **Analysis**:

  1. **Rc, not Arc.** The cached pool is wrapped in `Rc`,
     which is `!Send` and `!Sync`. Each thread sees a
     separate `Rc<Pool>` instance — there is no cross-
     thread sharing of the refcount. A Send-violation
     would be a compile error, not a runtime issue. The
     compiler enforces the invariant.

  2. **Detached tasks get their own runtime.**
     `detach_isolated` spawns a dedicated OS thread that
     mints a fresh compio runtime; any pool access from
     within that runtime initializes a fresh thread-local
     pool cache. No "borrowed pool from caller thread"
     anti-pattern.

  3. **Pool initialization is per-thread first-call**.
     Two concurrent first-calls on DIFFERENT threads
     each open their own pool independently. No
     synchronization needed; no race window. Memory cost
     scales with active-thread count, not call count.
     (Perf-r26's claim of "1 pool open per thread vs N
     per call" is correct.)

  4. **Pool teardown semantics under thread exit**: when
     a thread exits, the thread-local `Rc<Pool>` runs its
     destructor; the pool's connections close cleanly
     against pg. For long-lived threads (ntex workers,
     sweeper detached task) this never runs in steady
     state. For short-lived detached tasks
     (CreateGuard::drop) the pool opens-closes per task,
     which IS perf-suboptimal but NOT incorrect. (Perf-r27
     may want to revisit; concurrency lens raises no
     objection.)

  5. **No unsafe state reachable from the pool change**.
     The wake state machine's pg paths all check
     `Err(DatabaseError)` (per the R26-DEF1 carry); the
     pool-cache change does not introduce any new error
     class. Pool initialization failure routes to
     `RestoreHandlerError::Internal` → `Phase::Failed`
     → terminal write, same as the legacy pool-per-call.

- **Severity**: DEFER (no concurrency finding). Perf-r27
  owns the cost-per-call-on-short-lived-thread ask.

- **Carry mechanism**: NEW for r27 (cross-lens recheck
  confirms no concurrency angle).

## Round-briefing answers (summary)

The round briefing asked four specific questions. Direct answers:

### 1. Phase 2 race surface: new race between `submit_nomad_job` and driver's `stageDiskImages(taskConfig)`?

**No NEW race window introduced by Phase 2 capability landing.**

- The driver's `stageDiskImages` runs INSIDE `StartTask`
  BEFORE the CH-spawn step (per the ADR Phase 2 contract and
  the cited driver commit `b3b1fe59`). CH cannot spawn before
  workspace.img/home.img exist; the two are sequential within
  the same `StartTask` invocation on the same worker.
- `wait_for_alloc_running` polls Nomad's `ClientStatus`,
  which flips to "running" only AFTER `StartTask` returns —
  i.e., AFTER stageDiskImages completes AND CH spawns.
  Controller side never observes a "running" alloc with no
  workspace.img.
- The CreateGuard rollback runs on cleanup paths that purge
  the Nomad job (which kills the alloc → driver runs
  DestroyTask → driver's per-alloc cleanup runs). The
  Nomad job-purge serializes with StartTask-in-progress
  via Nomad's own task-lifecycle state machine.

**One concurrency adjacent shape**: the
`alloc_running_timeout_secs` budget (120s) now covers the
stageDiskImages work that previously ran pre-submit. See
R27-M2. NEUTRAL on correctness; minor operator-triage
observability shift.

**A different concurrency adjacent shape**: the `host_dir`
mtime baseline shifts from controller-submit-time to
driver-StartTask-time. See R27-I1. NEUTRAL for steady
state (600s grace dominates the 3-5s shift); LATENT for
failed-create observability.

### 2. R26-I1 (r3-A boot serial 5s await) — priority change post-Option C?

**Priority HOLDS** for the Phase 2 window. The boot path is
untouched by Phase 2. Forward-looking: if ADR Phase 5
disposition removes r3-A Constraints, the boot lookup's
blast radius collapses (no jobspec consumer) and R26-I1
demotes naturally. **But** R27-I2 argues Phase 5 should
KEEP Constraints for the per-user home.img persistence
invariant, in which case R26-I1's recommendation set
(doc-comment → /readyz surface → background-retry) remains
the right shape. **Net: HOLD, reassess at Phase 5
decision.**

### 3. R26-I2 (Phase::Ok terminal-overwrite divergence) — priority change?

**Priority FALLS.** Phase 2 does not close it (the failure
class is in WAKE not CREATE; Phase 2 only moves cold-boot
CREATE). But the Phase 3 cutover (cold-boot AND wake both
driver-staged per ADR section "Trade-offs" item 2) **DOES**
structurally close it: CreateGuard rollback becomes a no-op,
sweeper reaps driver-staged dirents not in-flight
controller-staged dirents, and the "Phase::Ok terminal-
overwrite while sandbox row already Running" failure class
has no remaining producer.

**Recommendation**: keep R25-I2 in the carry table as OPEN,
but mark "close at Phase 3 cutover, do NOT invest
test-coverage R&D NOW." The two-row CAS investment would
pay off only for the Phase 2 window; Phase 3 obsoletes it.

### 4. R25-I1 sweeper TOCTOU — sweeper obsoleted by Option C for cold-boot path?

**No.** The sweeper retains its role under Phase 2 AND
under Phase 3 cutover. Per ADR Phase 5 disposition table
line 335: "Still required for failed-create leak hygiene;
reaps driver-staged paths | KEEP, rewrite rustdoc to cite
driver as creator."

The sweeper's predicate (terminal sandbox + no pending
wake_jobs + mtime grace) is independent of WHO created the
host_dir. R25-I1's test-coverage ask remains valid and
correctly-shaped post-Phase-2. See R27-M3.

What DOES change:
- The mtime baseline (R27-I1).
- The docstring at sweep.rs:830-865 should cite the
  driver as creator in Phase 3 (per ADR Phase 5 line 335,
  deferred to Phase 3 landing).

## Items NOT findings (verified clean this round)

### [N/A] Phase 2 emission for cold-boot vs restore path

The emission split (`stage_disk_images = cfg.flag &&
restore_from.is_none()`) at nomad_ch.rs:2545-2589 is correctly
gated:

- Cold-boot + flag=true → `stage_disk_images: true` in
  Config + `zsbx_stage_disks: "true"` in Meta.
- Cold-boot + flag=false → both fields `false`/`"false"`.
- Cold-boot + flag=true + restore_from.is_some() → forced
  `false` (the restore branch consumes rootfs via
  RootfsSource hardlink/copy, never re-mkfs's; per ADR
  Phase 2 cold-boot-only contract).
- Restore-path emitter (build_restore_nomad_job_json):
  always emits `false` (R22-T1 parity test).

The four new tests at nomad_ch.rs:7438-7570 pin each of
these branches; restore_handler.rs:4353-4393 pins the
restore-path parity. Concurrency-r27 raises no objection.

### [N/A] CreateGuard's vm_index release

Unchanged from r26. The vm_index allocator is
`Arc<Mutex<VmIndexAllocator>>`; the CreateGuard's detached
cleanup task acquires the mutex via
`unwrap_or_else(|p| p.into_inner())` (poisoning-tolerant);
the release happens AFTER `purge_ok` confirms Nomad's
side is clean. Phase 2 does not touch this path.

### [N/A] BackendBuilder (R27-I1, df06d172)

The telescoping `from_config_*` chain is replaced with a
typed builder. The thread-of-control is identical: builder
collects `Option<…>` fields, `build()` consumes the builder
and returns `Backend::NomadCh(Arc<NomadCHBackend>)`. No
race introduced; the immutability invariant (`Arc` after
construction, no `get_mut`) is preserved. Confirmation
against r26-M1; the builder pattern is the legitimate
shape.

## Cross-lens consensus

- **arch-r27 owns** (1) the Phase 5 Constraints
  disposition (R27-I2 argues KEEP); (2) the R27-I1
  doc-comment/rename decision on `host_dir_created` field;
  (3) R26-I1 boot-lookup-shape carry from r26.
- **test-coverage r27 owns** (1) R25-I1's two-test pin
  (still valid post-Phase-2; do not let Phase 3 confuse the
  ask); (2) a guard-behaviour test for R27-I1 (~20 LOC
  asserting `host_dir_created==false` even when the driver
  may have staged); (3) a boot-failure path test from
  R26-I1 (carry, ~30 LOC).
- **perf-r27 owns** (1) R26-C1 thread-local-pool teardown
  cost on short-lived detached tasks (concurrency-r27
  raises no objection but flags as defer); (2) R26-I1
  timeout-value call (5s vs 1s); (3) Phase 2
  alloc_running latency-shift (R27-M2) — verify the 120s
  headroom is still comfortable under cold-cache stress.
- **code-quality r27 owns** the R27-M1 `host_dir_created`
  doc-comment / rename ask.
- **observability r27 owns** the R27-I1 missing-leak-log
  observability gap; the metric proposal
  (`sandbox_host_dir_orphaned_by_driver_total`) is its
  decision.
- **security r27**: no new vector. R22-S1 sanitize widening
  continues to cover both controller and driver path
  variants of `/var/zeroship/` paths.
- **Cluster T-8b-stress-r7** (in flight, scripts/-only):
  concurrency-r27 has no script-side objection. The
  capability flag default is `false`, so stress-r7's
  controller behaviour is identical to stress-r5/r6
  baselines. **A specific request**: if stress-r7 flips
  the flag (per `231e66c6` "flip driver_stages_disk_images=
  true" — this commit is OUTSIDE the audit scope because
  scripts/-only, but it does flip the cluster default to
  true for Phase 4 validation), the stress harness should
  also instrument `home.img` write-and-read across
  cycles for the same user to falsify R27-I2's
  multi-worker latent shape AHEAD of Phase 5.

## Carry table

| Finding | Source | r27 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry |
| R20-C1 (Failed side) | r20 → CLOSED r22+r23 e2e | fully CLOSED |
| R20-C1 / R25-I2 (Ok side) | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| R20-I2 sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 9th cycle** |
| R20-I3 / R21-I1 / R24-M2 Restoring watchdog | r20→r26 | **STILL OPEN — 9th cycle** (promote IF stress-r7 cold-cache stretches >60s) |
| R22-I1 observability-gap | r22 → CLOSED r23 | CLOSED carry |
| R22-M3 cross-controller claim | r22 minor | carry |
| R23-I1 (Failed side) | r23 → CLOSED r24 | CLOSED carry |
| R24-I1 fsync_dir doc-misframe | r24 NEW-IMP | **STILL OPEN — code-quality r27 owns** |
| R24-M1 sweep × machine race-into-terminal | r24 minor | carry |
| R24-M3 stranded taps (C-N-W2) | r24 minor | carry |
| R25-I1 sweeper TOCTOU invariant untested | r25 NEW-IMP | **STILL OPEN** (Phase 2 does NOT obsolete; carry to r28) |
| R25-I2 Phase::Ok terminal-overwrite divergence | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R25-M1 typed StagingPathMissing carry-through | r25 minor | carry (doc-comment ask) |
| R26-I1 boot-lookup serial-await + external-dep | r26 NEW-IMP | **STILL OPEN — PRIORITY HOLDS, reassess at Phase 5** |
| R26-M1 from_config_full threading consistent | r26 minor | CLOSED-with-watchful-eye (BackendBuilder confirms shape) |
| R26-M2 R25-T4 helper extraction race-clean | r26 minor | CLOSED-with-watchful-eye |
| R26-M3 read_snapshot_row DRY (concurrency angle) | r26 minor | carry (code-quality owns) |
| R26-DEF1 perf-r25 C1 concurrency recheck | r26 defer | CLOSED-with-watchful-eye (now Rc<Pool>; R27-DEF1) |
| **R27-I1** host_dir mtime baseline shifts (Phase 2 observability) | r27 NEW-IMP | NEW |
| **R27-I2** home.img per-user race preserved + Phase 5 latent | r27 NEW-IMP | NEW |
| R27-M1 host_dir_created semantic-overload | r27 minor | NEW |
| R27-M2 wait_for_alloc_running budget shift | r27 minor | NEW (operator-doc hygiene) |
| R27-M3 R25-I1 sweeper TOCTOU shape SHIFTS not closes | r27 minor | NEW (confirmation) |
| R27-DEF1 R26-C1 thread-local Rc<Pool> | r27 defer | NEW (cross-lens defer) |
| Older minors (R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1, R23-M1, R24-M5, R25-M2, R25-M3) | older minor | carry |

## Status block

```
Round 27 (Option C Phase 2 LANDED behind flag + R26-C1
          Rc<Pool> LANDED + R27-I1 BackendBuilder LANDED +
          stress-r7 IN FLIGHT):

  CLOSED:
    (none — no IMP/CRITICAL closed this round).

  STILL OPEN:
    R20-I2 (sweep host-scoping — IMPORTANT, arch; 9th cycle),
    R20-I3 / R21-I1 / R24-M2 (Restoring watchdog — MINOR; 9th
      cycle; promote to IMPORTANT IF stress-r7 surfaces
      Restoring-phase stretch >60s),
    R24-I1 (fsync_dir doc-misframe — IMPORTANT, code-quality),
    R25-I1 (sweeper TOCTOU invariant untested — IMPORTANT;
      Phase 2 does NOT obsolete this ask),
    R25-I2 (Phase::Ok terminal-overwrite divergence —
      IMPORTANT; PRIORITY FALLS; close at Phase 3 cutover),
    R26-I1 (boot lookup serial-await — IMPORTANT; PRIORITY
      HOLDS; reassess at Phase 5 disposition).

  NEW (r27):
    R27-I1 (Phase 2 host_dir mtime baseline shifts +
      observability gap on CreateGuard log line —
      IMPORTANT; arch-r27 owns doc-comment vs rename call),
    R27-I2 (home.img per-user race preserved under Phase 2
      + LATENT multi-worker breakage if Phase 5 removes
      r3-A Constraints — IMPORTANT; arch-r27 owns Phase 5
      Constraints disposition),
    R27-M1 (host_dir_created semantic-overload —
      observability hygiene; doc-comment ~5 LOC),
    R27-M2 (wait_for_alloc_running budget shift —
      operator-doc hygiene),
    R27-M3 (R25-I1 sweeper TOCTOU shape SHIFTS but does
      not close — confirmation; sweeper still required
      per ADR Phase 5),
    R27-DEF1 (R26-C1 Rc<Pool> concurrency recheck:
      Send/Sync clean, no cross-thread sharing,
      pool-cache thread-isolated — DEFER to perf-r27).

  CARRY:
    R19-I2, R19-I3, R19-I1 (production-unexercised 13th cycle),
    R20-M1..M5, R21-M1..M3, R22-M1, R22-M3, R23-M1, R24-M1,
    R24-M3, R24-M5, R25-M1, R25-M2, R25-M3, R26-M3.

  ASK:
    (1) test-coverage r27: ship R25-I1's two test pins
        (handler-side pg-gated + pure unit) — CARRY from r26.
    (2) test-coverage r27: guard-behaviour test for R27-I1
        (~20 LOC, fixture exercising try_create with
        driver-stage failure; assert host_dir_created==false
        even when driver staged the dir).
    (3) test-coverage r27: integration test for
        fetch_local_nomad_node_id boot-failure path
        (~30 LOC) — CARRY from r26.
    (4) arch r27: call on Phase 5 r3-A Constraints
        disposition (R27-I2 argues KEEP for home.img
        per-user invariant; arch lens has the final say).
    (5) arch r27: call on R27-I1 doc-comment vs rename
        for `host_dir_created` field (cheap path is
        doc-comment + cite ADR Phase 2 + Phase 3 retire-bool
        timeline; expensive path is rename now).
    (6) arch r27: call on R26-I1 shape — HOLD recommendation
        carries from r26 (doc-comment + /readyz surface;
        shape-change to mutable is the right long-term
        path; reassess at Phase 5).
    (7) arch r27: call on R25-I2 — STILL OPEN-PRIORITY-FALLS
        (close at Phase 3 cutover; do not invest two-row
        CAS R&D NOW).
    (8) code-quality r27: R24-I1 fsync_dir + R25-M1
        wake_machine + R26-M3 SnapshotRowMeta consolidation
        + R27-M1 host_dir_created doc-comment.
    (9) observability r27: R27-I1 metric proposal
        (`sandbox_host_dir_orphaned_by_driver_total`).
    (10) perf r27: R26-C1 Rc<Pool> teardown cost on
         short-lived detached tasks (concurrency-clean
         per R27-DEF1; cost is its own ask).
    (11) cluster T-8b-stress-r7: if `231e66c6` flips
         the flag default to true on the cluster, the
         stress harness should instrument `home.img`
         write-and-read across cycles per user to
         falsify R27-I2's multi-worker latent shape
         AHEAD of any Phase 5 Constraints removal.
```

## ASK clarifications for the user

Three open design questions concurrency-r27 raised but could not
resolve from code alone:

1. **R27-I2 Phase 5 Constraints disposition** — should the
   ADR Phase 5 disposition table reclassify r3-A
   Constraints from "REMOVE OR keep as defense-in-depth
   (decide post-stress)" to "KEEP for per-user home.img
   persistence (correctness, not defense-in-depth)"? The
   home.img per-user invariant is preserved under Phase 2
   only because Constraints pin all of a user's allocs to
   the controller's node. Removing Constraints
   simultaneously breaks the package-cache-persists
   contract unless home.img migrates to a shared store
   (which is a separate, larger investment).
   Concurrency-r27 recommends ARCH-r27 take this call BEFORE
   Phase 4 stress validation so the disposition is informed.

2. **R27-I1 `host_dir_created` rename vs doc-comment**:
   the field's name implies "is there a host_dir on disk?"
   but Phase 2 semantics make it mean "did the controller
   stage?" — operators triaging the CreateGuard log line
   at nomad_ch.rs:2244-2250 may misread. The cheap path
   is a 5-LOC doc-comment + cite ADR Phase 2 + Phase 3
   retire-bool timeline. The expensive path is rename now
   to `host_dir_controller_owned`. Concurrency-r27 prefers
   the cheap path (Phase 3 retires the bool anyway); arch
   owner-call.

3. **R25-I2 disposition** — recommend mark "STILL OPEN —
   close at Phase 3 cutover, do not invest two-row CAS R&D
   NOW" in the carry table. Phase 3 obsoletes the failure
   class structurally; investing in the CAS would pay off
   only for the Phase 2 window. User-confirm the
   disposition shift before r28.
