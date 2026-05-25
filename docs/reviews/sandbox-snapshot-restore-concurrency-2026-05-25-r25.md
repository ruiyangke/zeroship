# Sandbox/snapshot-restore — concurrency r25 review

Date: 2026-05-25 (UTC).
HEAD at audit: `a482f00d` (worktree
`.worktrees/sandbox-snapshot-restore`, branch
`feat/sandbox-snapshot-restore`). Round 25 of N. READ-ONLY.

Scope since r24:

- **v14/v34 bundle landed** (commits `d638b10f`, `e82bffd7`,
  `6b240683`, `c729c2b8`) — `host_dir` is now LEAKED on stop,
  reaped by a sweeper-owned GC at 5-min cadence / 1-hour mtime
  grace (`crates/sandbox/src/sweep.rs:909-1131`).
- **Typed staging-preflight error path landed** at `79871194`
  (`WakeErrorCode::StagingPathMissing` variant, wire code
  `staging_image_missing`) + `022f778a` (split
  `SubmitRestoreError::{Preflight, Other}` + new
  `RestoreHandlerError::StagingPreflight`) + `6476d18b` (backlog
  closure).
- **R24-I1 WARN context** landed at `1c255a00` (the
  `terminal_overwrite_blocked` WARN on `Phase::Failed` now carries
  `attempted_code` + `error_message` per concurrency-r24 R24-M4 and
  code-quality-r24 R24-I1 endorsement).
- **T-8b-stress-r3 in flight** (cluster lens, scripts/ only, no
  code overlap with this lens).

Prior: `…concurrency-2026-05-25-r24.md`.

## Summary

- **5 findings** (0 new CRITICAL, 2 new IMPORTANT, 3 MINOR).
- **R24-I1 (fsync_dir doc misframe) — STATUS NOTE**: arch-r25 §
  r25-A3 confirms d638b10f's commit message says the v33 fsync_dir
  was correct-but-redundant; the actual mechanism was
  cleanup-vs-retry. The fsync_dir doc-comment IS still misleading
  per arch-r25-A3. Concurrency lens carries the finding by
  reference; the lib-test (R24-I2 below) is the surface
  concurrency-r25 can verify directly.
- **R24-I2 (lib-test flake mechanism) — CLOSED THIS ROUND**. Ran
  `cargo test -p zeroship-sandbox --lib submit_restore_job --
  --test-threads=10` 5 consecutive iterations and the full lib
  suite (`455 passed`) at `--test-threads=10` 3 consecutive
  iterations on HEAD `a482f00d`. Zero flake reproductions. The
  flake mechanism remains unidentified, but the load-bearing
  observation is: post-`022f778a` the staging helper now uses the
  same `workspace_image_path` / `user_home_image_path` helpers as
  the production cold-boot path (per R25-I1 close note), which
  collapses one of r24's hypotheses — the staging-helper and
  production code now share a single path-derivation site rather
  than two. The flake may have been the divergent-path-string
  shape, which is now gone. Concurrency-r25 demotes from
  IMPORTANT to MINOR-with-watchful-eye. Carry to r26: if
  stress-r3 surfaces a new variant of "workspace.img missing" in
  the lib-test layer after the typed-error split, escalate. (See
  R25-M3.)
- **R25-I1 (NEW IMPORTANT) — Sweeper TOCTOU: invariant
  load-bearing, untested.** Arch-r25-A6 concern #2 names the
  window between gate-A (`get_sandbox_row`) and gate-B
  (`find_pending_wake_for_sandbox`) in `run_host_dir_gc_once`
  (sweep.rs:1038-1102). The window is sub-ms in real wall time,
  but the host_dir reap is destructive. The handler invariant
  "`POST /wake/{id}` refuses on terminal sandbox status BEFORE
  `insert_wake_job`" closes the window structurally — but **no
  test pins the invariant**, and the database-side
  `insert_wake_job` has no sandbox-row state predicate (the only
  enforcement is the handler's read-then-check at
  `admin_handlers.rs:1733-1771`). A future refactor that bypasses
  the handler's pre-flight (e.g., a new "force-replay terminal
  wake" admin RPC) would silently break the sweeper invariant.
  Lens-r25 owes a structural pin. See R25-I1.
- **R25-I2 (NEW IMPORTANT) — `Phase::Ok` write-then-guard
  divergence on R20-C1 fire**. When `WakeMachine::run` returns
  `Phase::Ok` and the terminal-state UPDATE no-ops because the
  sweep wrote `Failed` first, the **sandbox row is already
  `Running` and the vm_index reservation is still held**. The
  wake_jobs row says `failed/wake_worker_aborted`. From the
  client's perspective the wake failed; from the cluster's
  perspective there's a live restored VM at the derived agent_url.
  No cleanup fires after the guard trip on the Ok arm
  (wake_machine.rs:139-156). The invariant the R20-C1 guard
  preserves is **wake_jobs row coherence** — it does NOT enforce
  sandbox-row + vm_index ↔ wake_jobs coherence. This is the
  `R20-I3 / R21-I1` watchdog gap (R24-M2 carry) viewed from the
  Ok side. See R25-I2.

## CRITICAL

None this round. The wake state machine, R20-C1 guard, and v14/v34
sweeper bundle are concurrency-sound on the paths they were
designed for. The two IMPORTANT findings below are
invariant-pin asks (R25-I1) and a write-divergence shape (R25-I2),
not active-data-loss bugs.

## IMPORTANT

### [R25-I1] Sweeper TOCTOU invariant ("refuse wake for terminal sandbox BEFORE wake_jobs insert") is load-bearing but **never tested** — a future handler refactor could silently break the host_dir reap

- **Files**: `sweep.rs:1038-1102` (the two-SELECT TOCTOU window);
  `admin_handlers.rs:1728-1771` (the handler invariant); `db.rs:
  3127-3193` (`insert_wake_job` SQL — no sandbox-state predicate).
- **The window**: `run_host_dir_gc_once` runs
  ```
  T_A: row = get_sandbox_row(sid)          // 1 SELECT
  T_B: pending = find_pending_wake_for_sandbox(sid)  // 2nd SELECT
  T_C: remove_dir_all(<host_dir>)          // destructive
  ```
  Between T_A and T_B (sub-ms), a wake handler could theoretically
  insert a new `wake_jobs` row. If it does, the sweeper's
  follow-up SELECT misses it and the reap proceeds.
- **Why arch-r25-A6 says "narrows to zero"**: the wake POST
  handler at `admin_handlers.rs:1728-1771` reads
  `get_sandbox_row` and refuses any status that isn't
  `Snapshotted` / `SnapshottedSuspect`. Sandbox transitions are
  one-way out of terminal states:

  | from              | to (any)                                            |
  |-------------------|-----------------------------------------------------|
  | Stopped           | (none — terminal)                                   |
  | Lost              | (none — terminal)                                   |
  | Orphan            | (none — terminal)                                   |
  | Snapshotted       | Restoring (wake_machine.rs:285); deleted only       |
  | Restoring         | Snapshotted (rollback) / Running (success) / SnapshottedSuspect (corrupt) |
  | RestoringCold     | SnapshottedSuspect (transient sweep, sweep.rs:135)  |
  | Snapshotting      | SnapshottingAborted (transient sweep, sweep.rs:133) |

  Verified via `Grep update_sandbox_status.*Snapshotted` and
  `Grep recovery_target`: **no code path transitions FROM
  `Stopped` / `Lost` / `Orphan` to any non-terminal state**. So a
  wake handler that observes `Snapshotted` at T_Y > T_A on a
  sandbox that was `Stopped` at T_A is structurally impossible.
- **Why this is still an IMPORTANT finding**: the invariant
  depends on TWO things being simultaneously true forever:
  1. The handler's read-then-check pre-flight is the SOLE
     enforcement of the "no wake_jobs row for terminal sandbox"
     property.
  2. No code path transitions FROM `{Stopped, Lost, Orphan}`
     OUT.
  **Neither is structurally enforced by the type system or the
  database.** A new admin RPC (e.g., "force-replay terminal wake"
  for operator triage) that bypasses the handler's pre-flight, or
  a new transient-recovery target (e.g., "Lost → Snapshotted" if
  we ever decide a lost-but-recoverable row should be wake-able)
  would silently break the sweeper.
- **The reap is destructive**: a wrongly-reaped host_dir means
  `workspace.img` is gone. The next wake POST would hit the
  R25-S1 staging-preflight rejection (typed
  `WakeErrorCode::StagingPathMissing`) and the sandbox would be
  rolled back to `Snapshotted` (or `SnapshottedSuspect`). Data
  loss IS bounded by the snapshot's content-addressed blob in
  object storage, but the user-visible symptom is "wake failed,
  please retry" — and a retry produces the same failure until a
  full cold-boot recreates `workspace.img`. **Recoverable, but
  not silent**.
- **What test would catch a future violation?** Two layers:
  1. **PG-gated invariant test** for the handler (new file under
     `crates/sandbox/tests/sandbox_pg_e2e.rs`):
     ```rust
     // For every terminal sandbox status, a POST /wake/{sid}
     // returns 409 state_mismatch AND inserts zero wake_jobs rows.
     #[test] async fn wake_post_refuses_terminal_sandbox_and_inserts_nothing() {
         for status in [SandboxStatus::Stopped, SandboxStatus::Lost,
                        SandboxStatus::Orphan] {
             let sid = uuid::Uuid::now_v7();
             insert_sandbox_row(&db, sid, status).await;
             let resp = post_wake_inner(&state, sid).await;
             assert_eq!(resp.status(), 409);
             let count = count_wake_jobs(&db, sid).await;
             assert_eq!(count, 0, "wake_jobs leaked for terminal {status:?}");
         }
     }
     ```
     This pins enforcement #1.
  2. **Pure unit test** for the transition table (extend the
     existing `sweep::unit_tests::recovery_target_pins_proposal_table`
     test at `sweep.rs:1421-1439`):
     ```rust
     #[test] fn terminal_sandbox_statuses_have_no_outgoing_transition() {
         // Pin: terminal statuses (Stopped/Lost/Orphan) are NEVER
         // the source of any transient-recovery transition.
         for terminal in [SandboxStatus::Stopped, SandboxStatus::Lost,
                          SandboxStatus::Orphan] {
             assert_eq!(recovery_target(terminal), None,
                 "terminal status {terminal:?} must NOT have a recovery target");
         }
     }
     ```
     This pins enforcement #2. (Already partially pinned for
     `Running` / `Snapshotted` at `sweep.rs:1437-1438`; extend to
     the terminal triad.)
  3. **(Stronger, optional) DB-side defense-in-depth**: add an
     `AND status NOT IN ('stopped','lost','orphan')` predicate to
     `insert_wake_job`'s SQL with a FK-shaped join to
     `sandbox.sandboxes`. Cost: 1 extra subquery per insert (the
     wake POST path is rate-limited and already does
     `get_sandbox_row`, so the second SELECT is moot — but the
     atomic enforcement at INSERT-time is stronger than the
     handler's TOCTOU). Compio-pg-postgres has no compile-time
     SQL-checker, so this is the only way to make the invariant
     structural. **Concurrency-r25 hand-off: arch-r25 owns the
     defense-in-depth call.**
- **Carry mechanism**: arch-r25-A6 concern #2 flagged TOCTOU as
  "concurrency territory; pin invariant in test." Concurrency-r25
  takes the hand-off: tests (1) and (2) above are the pin.
- **Severity**: IMPORTANT — the invariant is sound TODAY but
  un-anchored. The sweeper's destructive write makes silent
  drift dangerous.
- **Owner**: test-coverage r25 (the two tests above) +
  arch-r25 (call on DB-side defense-in-depth).

### [R25-I2] `Phase::Ok` terminal write blocked by sweep → sandbox row already `Running` + vm_index held → cluster diverges from wake_jobs row

- **Files**: `wake_machine.rs:128-156` (the `Phase::Ok` terminal
  write path); `wake_machine.rs:516-548` (where sandbox row is
  CASd to `Running` BEFORE the terminal wake_jobs write); `db.rs:
  3406-3438` (`claim_orphan_wake_for_recovery`).
- **The divergence shape**:
  ```
  WakeMachine::run sequence (success path):
    t0: reserve_vm_index            // vm_index reserved
    t1: store.get + config + submit // alloc spawned
    t2: wait_for_livez              // agent responsive
    t3: clock_resync                // CLOCK_REALTIME aligned
    t4: register_restored           // backend registry inserted
    t5: update_sandbox_status(Running, g1, None)  // sandbox row Running
    t6: clear_snapshot_metadata     // snapshot fields cleared
    return Phase::Ok { vm_index, agent_url }

  WakeMachine::drive (after run returns Ok):
    t7: update_wake_job_state(Ok, agent_url, ...)
        // SQL guard: AND state NOT IN ('ok','failed')
        // IF rows_affected == 0 → R20-C1 guard tripped → WARN + counter
  ```
  Between t6 and t7 (sub-ms), the sweep's
  `claim_orphan_wake_for_recovery` could write
  `state='failed', error_code='wake_worker_aborted'` if the
  takeover threshold has elapsed (default 60s, floor 30s).
- **The R20-C1 guard is correct on the wake_jobs row** — the
  sweep's terminal write wins, the machine's `Ok` write no-ops,
  the wake_jobs row contains the sweep's verdict.
- **But the sandbox row is already `Running`**. The CAS at line
  519 succeeded at t5. The vm_index is reserved. The agent is
  live at `agent_url`. From the cluster's perspective the
  sandbox is alive; from the wake_jobs row a client polling sees
  `state=failed, error=wake_worker_aborted, message=…`. The
  client concludes wake failed; the next request to the agent
  would succeed (the agent is up, listening, registered).
- **Practical likelihood**: the takeover threshold default is 60s
  (floor 30s). The wake machine's t0→t6 wall time on a happy path
  is dominated by `wait_for_livez` (default 30s) + `store.get`
  on cold cache (~tens of seconds for 1GB) + `clock_resync`
  (~ms). Cold-cache wakes near the threshold are realistic; warm
  cache wakes are sub-second and safe. Stress-r24 wakes were all
  sub-threshold (per concurrency-r24 R24-M2 reading); stress-r25
  (in flight) may pressure.
- **What concurrency-r24 R24-M2 / R21-I1 watchdog would prevent
  (re-statement)**: a `lessee_updated_at = now()` heartbeat
  during the `store.get` blocking call (currently no heartbeat
  per concurrency-r24 R24-M2). Without it, a cold-cache restore
  can stretch past the threshold and lose the wake_jobs row to
  the sweep — even while the underlying restore is making
  progress. **This is the same finding R20-I3 has carried for 7
  cycles; concurrency-r25 endorses promotion to IMPORTANT for r26
  IF stress-r25 reproduces.**
- **Wider question**: is the Ok-terminal-blocked branch a "we'll
  just log a WARN and let the operator triage" shape (current),
  or should it actively roll back the sandbox row from
  `Running` → `Snapshotted` and release the vm_index? Three
  options:
  1. **Current behavior** (log + counter, no cleanup): the
     sandbox keeps running, the client retries via fresh POST
     /wake, the new POST observes `Running` and returns 409
     `state_mismatch` (or — if the snapshot-restore mode permits
     a wake on a Running sandbox — the client adopts the live
     agent_url via a different RPC). **Operator triage is the
     fallback.** The vm_index is reserved; the sandboxes row is
     authoritative; the wake_jobs row is a misleading audit
     breadcrumb.
  2. **Rollback the sandbox row** to `Snapshotted` + release
     vm_index: matches the failed-shape state machine but DROPS
     A WORKING VM the client may already have RPC'd against. Bad.
  3. **Atomic two-row CAS** at t5+t7 (single transaction
     wrapping both UPDATEs): the only way to keep the rows
     coherent. Compio-pg-postgres supports `BEGIN`/`COMMIT`;
     wrapping the two writes in a single tx with a recheck on
     the wake_jobs state inside the tx would let the machine
     **observe the sweep's terminal write at commit time** and
     fail loudly. ~30 LOC. The cost is one extra round-trip per
     successful wake.
- **Concurrency-r25 recommendation**: option (3). Treat the Ok
  arm with the same transactional discipline as the failure arm
  (the failure arm is already rollback-safe). The watchdog
  (R20-I3 / R24-M2) is the upstream prevention; option (3) is
  the downstream defense.
- **Severity**: IMPORTANT. The divergence is recoverable
  (sandbox is alive; the next request works; the wake_jobs row's
  audit breadcrumb is wrong but doesn't compromise data
  integrity). But it's a coherence violation that a polling
  client surfaces as user-visible failure on a successful wake.
- **Cross-lens**: arch-r25 owns the design call on (1) vs (3).
  Test-coverage r25 owes a pg-gated test that drives the exact
  shape (machine writes Ok while sweep races terminal failed;
  assert sandbox_row.status == Running AND
  wake_terminal_overwrite_blocked counter bumped). This would be
  the SUCCESS-side mirror of R23-I1's existing pair.
- **Carry mechanism**: this is the unwritten half of R20-C1. The
  failed-side (R22-I1 + R23-I1) is closed; the Ok-side has the
  same shape but isn't tested.

## MINOR

### [R25-M1] Typed `StagingPathMissing` carry-through is atomic on `agent_url` column — verified by code inspection

- **Files**: `wake_machine.rs:691` (`classify_failure` mapping);
  `wake_machine.rs:606-627` (`rollback_and_classify`);
  `db.rs:3255-3296` (`update_wake_job_state` SQL — `agent_url =
  COALESCE($4::TEXT, agent_url)`); `wake_machine.rs:560-585`
  (`set_state` — passes `None` for agent_url on every
  intermediate transition).
- **The pin task name**: "is the carry-through atomic? Can a
  partially-completed `submit_restore_job` produce a state where
  the error code is set but `agent_url` is non-NULL?"
- **Analysis**:
  1. `agent_url` is only written by `Phase::Ok` (line 135
     `Some(agent_url.as_str())`). Every other call site passes
     `None`.
  2. `None` + `COALESCE($4::TEXT, agent_url)` → preserves the
     existing column value.
  3. The column default is NULL on insert (db.rs:3138-3142).
  4. The CHECK constraint
     `wake_jobs_agent_url_nullable_only_when_ok_or_failed`
     (verified at `crates/sandbox/migrations/0010_*` per
     `wake_jobs_agent_url_check_constraint_enforced` pg-gated
     test at `tests/sandbox_pg_e2e.rs:4216`) requires
     `state IN ('ok','failed') OR agent_url IS NULL`.
  5. For `submit_restore_job` → `Preflight` → `StagingPreflight`
     → `Phase::Failed { code: StagingPathMissing, message: … }`:
     the failed-arm UPDATE at `wake_machine.rs:173-181` passes
     `None` for agent_url. COALESCE preserves NULL.
  6. **Result**: at the moment `state` flips to `failed`, the
     `agent_url` column is still NULL. The CHECK constraint
     accepts `failed` + NULL agent_url. **The carry-through is
     atomic.** ✓
- **Edge case attempted**: could a prior in-flight wake on the
  SAME `wake_id` have set `agent_url` before this terminal
  write? **No**: `wake_id` is fresh per wake POST (line 1781
  `new_wake_id()`). Even if the same `sandbox_id` is involved,
  each POST mints a new typed wake_id. The replay path returns
  the existing wake_id (line 1707) but does NOT spawn a fresh
  machine. So the `wake_id`'s `agent_url` history is monotonic:
  NULL → (NULL preserved by all in-flight writes) → either NULL
  (failed terminal) or set-once (Ok terminal). No accidental
  prior set.
- **Severity**: MINOR (confirms the typed carry-through is
  safe; documents the invariant for future readers).
- **Recommendation**: ~3 LOC doc-comment add at
  `wake_machine.rs:165-172` (above the `Phase::Failed` UPDATE)
  pinning the invariant:
  > "`agent_url=None` here is load-bearing: combined with
  > `COALESCE` semantics in `update_wake_job_state` it preserves
  > the column at NULL. The CHECK constraint
  > `wake_jobs_agent_url_nullable_only_when_ok_or_failed`
  > validates the resulting (state='failed', agent_url=NULL)
  > shape. R25-M1: do not change to `Some("")` without also
  > revisiting the CHECK constraint."

### [R25-M2] `AdminRole::ReadOnly` read consistency on `GET /wake/{id}` — single SELECT, no read-skew window

- **Files**: `admin_handlers.rs:1882-1955` (`poll_wake`); `db.rs:
  3197-3216` (`get_wake_job`).
- **The question**: "with T1 landed, an RO bearer can issue GET
  /wake/{id} concurrent with the wake-machine updating the row.
  Is the read consistent (single SELECT) or does it pull from
  multiple sources?"
- **Analysis**: `get_wake_job` is a single `query_opt`
  (`SELECT … FROM sandbox.wake_jobs WHERE wake_id = $1`). No
  composition with other tables, no separate timestamp lookup,
  no follow-up SELECT. Under READ COMMITTED (the workspace
  default; verified in r24 review by inspection of the pool
  configuration), the SELECT returns the row as of statement
  start. The wake-machine's UPDATE either completes before the
  SELECT (RO observes post-update state) or after (RO observes
  pre-update state). **No skew possible** — the row is the
  atomic unit and the SELECT is single-statement.
- **What about the `sandbox_id` path-mismatch check** at line
  1945? `row.sandbox_id` came from the single SELECT; it's
  internally consistent with the row's other fields (state,
  agent_url, etc.). No second read.
- **Edge case**: what if the wake-machine is mid-update at the
  exact moment of SELECT? The pg row-level `FOR UPDATE` lock
  serializes the WRITE side; the READ side under READ COMMITTED
  does NOT take the row-lock and instead reads the **last
  committed version** (MVCC). So RO never blocks; it sees either
  pre-write or post-write. **Strictly consistent per-row,
  weakly consistent across rows (but `poll_wake` only reads
  one).**
- **Severity**: MINOR (no concern — documenting the read
  consistency story for the lens record).
- **No action required.**

### [R25-M3] R24-I2 lib-test flake — NOT REPRODUCED at HEAD `a482f00d`; demote to MINOR with carry

- **Files**: `restore_handler.rs:3041-3068`,
  `restore_handler.rs:2895-2924`, `restore_handler.rs:2929-2948`
  (the three flake-suspect tests).
- **Test runs at HEAD `a482f00d`** (this round):
  - `cargo test -p zeroship-sandbox --lib submit_restore_job --
    --test-threads=10`: 5 consecutive runs, **5/5 GREEN**, each
    finishing in ~1.0s (5 passed; 451 filtered).
  - `cargo test -p zeroship-sandbox --lib -- --test-threads=10`:
    3 consecutive full-suite runs, **3/3 GREEN**, each finishing
    in ~3.9s (455 passed; 1 ignored; 0 failed).
- **Likely closure mechanism (post-r24)**:
  - r24 hypothesised dentry-cache / inter-test interference.
  - At `022f778a` the staging helper switched from inline
    `host_dir.join("workspace.img")` to the production
    `workspace_image_path(&host_dir)` / `user_home_image_path`
    helpers (R25-I1 close note). Both production AND
    `stage_disk_image_preconditions` now derive the path from
    the same helper. **If the flake mechanism was a string-
    derivation mismatch between the staging helper and the
    production assertion, the convergence closes it.**
  - This is a hypothesis, not a proof; the flake may simply have
    been timing-sensitive and the new test harness wall-time is
    different.
- **Severity**: MINOR (demoted from IMPORTANT in r24). Carry
  with a watchful eye: if stress-r3's lib-test run surfaces a
  fresh flake variant on the same tests, escalate back to
  IMPORTANT.
- **Recommendation**: leave the tests as-is; if a flake recurs,
  add `serial_test::serial` + tracing-instrumented timestamps
  per concurrency-r24 R24-I2's bisection plan. Do NOT
  preemptively add the serial gate — it would mask the
  mechanism if it returns.

## Items NOT findings (verified clean this round)

### [N/A] `taps_orphaned_total` counter — controller aggregation

- **Driver v14** (cross-worktree, `177ff165`) adds
  `tapsOrphanedTotal` as an in-process Go counter inside the
  Nomad driver plugin. The **controller side does NOT aggregate
  it** — verified via `Grep -r taps_orphaned_total crates/` →
  0 matches.
- Per stress-r3 cluster review note: there's no transport for
  this counter; it's currently unreachable from the controller's
  prometheus surface. arch-r25-A2's kernel-state-surface
  enumeration ADR is the parent finding. **Concurrency-r25
  raises no objection** — this is a metrics-transport gap, not a
  concurrency defect. The counter's contents are per-process /
  per-driver-instance; aggregation belongs in the cluster
  review's observability ask, not in the wake state machine.

### [N/A] R23-I1 e2e tests (pg-gated) — counter monotonicity

- The `failed_to_ok` / `ok_to_failed` pg-gated e2e tests landed
  at `234c3bdf` (r24) remain intact and exercise the R20-C1
  guard end-to-end. The tests file
  (`crates/sandbox/tests/sandbox_pg_e2e.rs::wake_machine_e2e`)
  has zero diff this cycle. **Closure verdict: CLEAN, carry**.

## Cross-lens consensus

- **arch-r25-A6 concern #2 → R25-I1**: arch hand-off accepted.
  Concurrency-r25 names the two test-shape pins (handler-side +
  pure unit) that would catch a future violation. Arch-r25 owns
  the call on DB-side defense-in-depth.
- **arch-r25-A1 (path-leak via verbatim driver msg + RO bearer
  composition)**: NOT a concurrency finding. Routes through
  security-r25-S1.
- **R20-I3 / R21-I1 / R24-M2 watchdog** (lessee heartbeat during
  store.get): **STILL OPEN — 7th cycle**. R25-I2 names the
  Ok-side divergence shape that the watchdog would prevent.
  Concurrency-r25 endorses promotion to IMPORTANT for r26 IF
  stress-r3 surfaces a Restoring-phase stretch >threshold (cold-
  cache restore at fleet scale, fresh worker, no warm blobs).
- **R20-I2 cross-controller claim semantics**: STILL OPEN, 7th
  cycle on the carry table. arch-r25 owns; concurrency-r25
  raises no new objection.
- **R24-I1 fsync_dir doc-misframe**: arch-r25-A3 vindicates the
  diagnosis (d638b10f's commit message confirms the v33
  fsync_dir was correct-but-redundant). Doc-comment still
  misleading per arch-r25-A3. **Code-quality r25 owns the
  doc-comment tighten**; concurrency-r25 confirms the
  cross-process visibility claim is structurally wrong and the
  syscall-barrier reading is correct.
- **R24-I2 lib-test flake**: CLOSED-with-watchful-eye this
  round (R25-M3). If stress-r3 surfaces a fresh variant,
  escalate.

## Lens hand-off

- **Test-coverage r25** (P0): the two tests pinning R25-I1's
  invariant. One pg-gated (handler refuses terminal sandbox +
  zero wake_jobs row inserted) + one pure unit
  (`recovery_target` exhaustiveness for the terminal triad).
  ~30 LOC total.
- **Architecture r25** (P0): call on R25-I1 enforcement layer
  (handler-only TOCTOU vs. DB-side defense-in-depth at
  `insert_wake_job`). Recommend: DB-side, since the wake-POST
  handler is rate-limited and one extra SQL predicate is cheap.
- **Architecture r25** (P1): call on R25-I2 — current behavior
  (log + counter, no cleanup) vs. atomic two-row CAS via
  transaction. Recommend: atomic. ~30 LOC, one extra round-trip
  per successful wake. The success path is the dominant case
  but the failure window is the operator-pain case.
- **Code-quality r25**: R24-I1 fsync_dir doc-comment tighten
  (carry from r24; arch-r25-A3 vindicates). ~5 LOC. R25-M1
  doc-comment add at wake_machine.rs:165-172. ~3 LOC.
- **Performance r25**: no concurrency objection. R25-I2's
  atomic-two-row-CAS recommendation adds one round-trip per
  successful wake (~ms). Acceptable cost.
- **Security r25**: R25-A1 path-leak vector (arch-r25's CRITICAL)
  is security territory. Concurrency-r25 raises no parallel
  finding.
- **Cluster T-8b-stress-r3** (in flight, scripts/-only):
  concurrency-r25 has no script-side objections. The bundle
  closes the host_dir leak vector that was the proximate cause
  of stress-r2's 33% race-win rate per arch-r25-A1.

## Carry table

| Finding | Source | r25 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry; stress-r24 confirms |
| R20-C1 terminal-overwrite (Failed side) | r20 → CLOSED r22 + r23 e2e | **fully CLOSED**; data + observability + tests |
| **R20-C1 (Ok side) — R25-I2 NEW** | r25 NEW-IMP | NEW (this round); see R25-I2 |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| **R20-I2** sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 7th cycle** |
| **R20-I3 / R21-I1 / R24-M2** Restoring watchdog | r20 → r21 → r22 → r23 → r24 → r25 | **STILL OPEN — 7th cycle**; R25-I2 names the Ok-side divergence shape |
| R22-I1 observability-gap | r22 → CLOSED r23 | CLOSED carry |
| R22-M3 cross-controller claim | r22 minor | carry |
| R23-I1 terminal→terminal e2e (Failed side) | r23 → CLOSED r24 (`234c3bdf`) | CLOSED carry |
| R24-I1 fsync_dir doc-misframe | r24 NEW-IMP | **STILL OPEN — code-quality r25** (arch-r25-A3 vindicates) |
| **R24-I2 lib-test flake** | r24 NEW-IMP | **CLOSED-with-watchful-eye** (R25-M3); not reproduced at HEAD `a482f00d` |
| R24-M1 sweep × machine race-into-terminal | r24 minor (doc-pin) | carry |
| R24-M3 stranded taps (C-N-W2) | r24 minor (out-of-tree) | carry; arch-r25-A1 owns the path-leak composition |
| R24-M4 Phase::Failed Ok(0) log payload | r24 minor | **CLOSED at `1c255a00`** (error_code + error_message threaded into WARN) |
| R24-M5 detach migration sound | r24 minor | CLOSED carry |
| **R25-I1** sweeper TOCTOU invariant untested | r25 NEW-IMP | NEW |
| **R25-I2** Phase::Ok terminal-overwrite divergence | r25 NEW-IMP | NEW |
| R25-M1 typed StagingPathMissing carry-through atomic | r25 minor | NEW (confirmation; doc-comment ask) |
| R25-M2 AdminRole::ReadOnly read consistency | r25 minor | NEW (confirmation, no action) |
| R25-M3 R24-I2 lib-test flake | r25 minor (was r24 IMP) | DEMOTED; carry |
| R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1 | older minor | carry |

## Status block

```
Round 25 (v14/v34 bundle LANDED + typed StagingPathMissing LANDED +
          R24-I1 WARN context LANDED + stress-r3 IN FLIGHT):
  CLOSED:
    R24-M4 (1c255a00; WARN now carries error_code + error_message),
    R24-I2 (R25-M3 demotion; not reproduced at HEAD a482f00d at
            --test-threads=10, 5x + 3x full-suite runs).
  STILL OPEN:
    R20-I2 (sweep host-scoping — IMPORTANT, arch; 7th cycle),
    R20-I3 / R21-I1 / R24-M2 (Restoring watchdog — MINOR; 7th
      cycle; R25-I2 names the Ok-side divergence the watchdog
      would prevent — promote to IMPORTANT for r26 IF stress-r3
      surfaces Restoring stretch >threshold),
    R24-I1 (fsync_dir doc-misframe — IMPORTANT, code-quality;
      arch-r25-A3 vindicates).
  NEW (r25):
    R25-I1 (sweeper TOCTOU: invariant load-bearing but never
      tested; arch-r25-A6 concern #2 hand-off accepted;
      concurrency-r25 names the two-test pin),
    R25-I2 (Phase::Ok terminal-overwrite divergence: sandbox
      row Running + vm_index reserved + agent live, but
      wake_jobs row failed; recommendation: atomic two-row CAS),
    R25-M1 (typed StagingPathMissing carry-through atomic on
      agent_url column — verified; doc-comment ask),
    R25-M2 (AdminRole::ReadOnly read consistency on
      GET /wake/{id} — single SELECT, no skew; no action),
    R25-M3 (R24-I2 demoted; lib-test flake not reproduced).
  CARRY:
    R19-I2, R19-I3, R19-I1 (production-unexercised 11th cycle),
    R20-M1..M5, R21-M1..M3, R22-M1, R22-M3, R23-M1, R24-M1,
    R24-M3, R24-M5.

  ASK:
    (1) test-coverage r25: ship R25-I1's two test pins
        (handler-side pg-gated + pure unit). ~30 LOC.
    (2) arch r25: call on R25-I1 enforcement layer
        (handler-only vs DB-side defense-in-depth at
        insert_wake_job). Recommend DB-side.
    (3) arch r25: call on R25-I2 — current log+counter behavior
        vs atomic two-row CAS via tx. Recommend atomic.
    (4) code-quality r25: R24-I1 fsync_dir doc-comment tighten
        (~5 LOC) + R25-M1 wake_machine.rs agent_url invariant
        doc-comment (~3 LOC).
    (5) R20-I3 / R21-I1 watchdog: re-evaluate after stress-r3 —
        promote to IMPORTANT if cold-cache restore stretches
        Restoring past 60s.
    (6) arch r25: R20-I2 cross-controller claim (7th cycle).
```

## ASK clarifications for the user

Three open design questions concurrency-r25 raised but could not
resolve from code alone:

1. **R25-I1 enforcement layer**: should the "no wake_jobs row
   for terminal sandbox" invariant be enforced at the handler
   pre-flight (current, TOCTOU-vulnerable), at the DB INSERT
   (atomic, +1 SQL predicate), or BOTH (defense-in-depth)? The
   handler is rate-limited so the DB-side cost is moot; the
   atomic enforcement at INSERT-time is stronger. **Recommend:
   BOTH** (keep the handler check as a fast-path optimization,
   add the DB predicate as the structural truth).

2. **R25-I2 design call**: when `Phase::Ok` returns and the
   wake_jobs UPDATE no-ops because the sweep raced to terminal
   failed, what's the right behavior?
   - (1) **Current**: log + counter, no cleanup. Operator
     triage the divergence. Sandbox stays alive.
   - (2) Roll back sandbox row + release vm_index. **DROPS A
     WORKING VM**; bad.
   - (3) Atomic two-row CAS via single tx wrapping the
     sandbox-row CAS + wake_jobs UPDATE; recheck wake_jobs state
     inside the tx; fail loudly if the sweep raced.
   Concurrency-r25 recommends (3). The watchdog (R21-I1) is the
   upstream prevention; (3) is the downstream defense.
   **Question for user/arch**: does the +1 round-trip per
   successful wake (typical ~ms) buy enough coherence to justify
   the cost? On the SLO budget for wake p50 it's negligible; on
   the SLO budget for wake p99.9 it's a fixed tail cost.

3. **R20-I3 / R21-I1 watchdog 7th cycle**: stress-r24 didn't
   pressure (wakes were sub-threshold). Stress-r3 (in flight)
   may. If stress-r3 surfaces a single Restoring-phase >60s,
   the watchdog moves from "defer-acceptable" to "must-land
   before T-8b cutover." Concurrency-r25 reads the watchdog
   shape as: a periodic `lessee_updated_at = now()` heartbeat
   spawned alongside the `store.get` blocking call, cancelled
   on Ok/Failed terminal. ~20 LOC; one new pg-gated test for
   the lessee-not-stolen-during-long-store invariant. Awaits
   stress-r3 data.
