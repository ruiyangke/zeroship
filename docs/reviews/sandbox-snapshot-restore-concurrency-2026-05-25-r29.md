# Sandbox/snapshot-restore — concurrency r29 review

Date: 2026-05-25 (UTC).
HEAD at audit: `b8310356` (worktree
`.worktrees/sandbox-snapshot-restore`, branch `master`).
Round 29 of N. READ-ONLY.

Scope since r28 (`5a0647c3` → `b8310356`, sandbox-crate only):

- **R28-C1 fix** (`9e1f6276`) — `CreateGuard::drop` inlines
  `compio::time::sleep(release_delay).await` + `release()` inside
  the cleanup future itself (Option A from the r28 brief), replacing
  the `spawn_delayed_release(...)` call at that site only. The
  helper itself is unchanged; the rustdoc on the inline path warns
  against unifying the two call sites by changing the helper.
- **R28-I1 + R28-I2 fix** (`d00f12dd`) — T5
  `verify_agent_version_post_restore` + `clock_resync_post_restore`
  parallelised via `futures::join!(t5_future, clock_resync_future)`
  at `wake_machine.rs:529-530`. Half-dead-agent fingerprint
  surfaces as structured `transport_error: bool` on both
  `VersionCheckOutcome::Skipped` and the new
  `ClockResyncOutcome::Err` (returned by the new
  `clock_resync_post_restore_typed` wrapper). Paired-fail (both
  transport-errored against the same agent_url) routes to a
  distinct WARN with `target="sandbox::wake::half_dead_agent"` and
  a `ClockResyncFailed` rollback whose `message` carries the
  `"half_dead_agent: "` prefix.
- **r29 paperwork** (`ce062846`) — architecture r29, performance
  r28, api-surface r28 reviewer artefacts. Architecture-r29 raised
  r29-A2 (CRITICAL): `spawn_delayed_release` still pub with the
  detach_isolated trap inside; the round-41 brief asks
  concurrency-r29 to verify there is no ANOTHER detach_isolated
  call site currently planting a delayed task that gets silently
  dropped.
- **stress-r9 cluster** (`def11cb4`, `2468ab96`, `5a0647c3`,
  `baf1b78c`, `a3cfca10`, `508c3d76`, `0bbdae94`, `b8310356`) — out
  of scope (scripts only, no production-code delta).

In flight: stress-r9-retry cluster cycle (scripts-only per
briefing); architecture r29 closure pass.

Prior: `…concurrency-2026-05-25-r28.md`.

## Summary

- **6 findings** this round (**1 NEW CRITICAL**, 1 carry/confirmation
  cluster, 4 new MINOR/observations).
- **R29-C1 (NEW CRITICAL — same shape as R28-C1, sibling call site)**:
  the snapshot admin handler (`admin_handlers.rs:1491-1506`) wraps
  `state.backend.teardown_source_for_snapshot(sandbox_id).await`
  inside `detach_isolated("snap-teardown-<tail>", ...)`. The future
  body is purely the await (no surrounding loop, no follow-up await
  to extend runtime lifetime). `teardown_source_for_snapshot` →
  `NomadCHBackend::stop_preserving_state` → `stop_inner(.., false)`
  → at `nomad_ch.rs:1324` calls
  `VmIndexAllocator::spawn_delayed_release(...)`. That helper plants
  `compio::runtime::spawn(async { sleep(5s); release; log }).detach()`
  on the CURRENT runtime — which here is the snap-teardown private
  compio runtime minted by `detach_isolated`. As soon as
  `teardown_source_for_snapshot.await` returns Ready, `block_on`
  returns; the runtime is dropped; `Scheduler::clear()` discards the
  pending delayed-release future (compio-runtime 0.11.0
  `runtime/mod.rs:389-399`); **vm_index is leaked** until the
  next-controller-boot orphan prune. Same correctness defect r28
  closed at `CreateGuard::drop`, on the sibling
  `teardown_source_for_snapshot` admin call site. NOT closed by the
  R28-C1 inline-sleep+release fix (which targeted CreateGuard::drop
  only).
- **R29-I1 (NEW IMPORTANT, observational — architecture-r29-A2
  confirmed)**: `VmIndexAllocator::spawn_delayed_release` (`pub`,
  `nomad_ch.rs:363-387`) remains a public footgun. Any future
  `detach_isolated`-spawned future that ends with a call routing
  through `stop_inner` (or `stop`) will silently reproduce R28-C1 /
  R29-C1. The R28-C1 fix's rustdoc explicitly warned against
  unifying the two call sites by changing the helper — but the
  helper's NAME (`spawn_delayed_release`) and SIGNATURE
  (fire-and-forget, no JoinHandle) actively invite the
  R28-C1-shaped misuse. Two call sites in production: stop_inner
  (`:1324`) for the long-lived `stop()` path; CreateGuard::drop
  inlines instead. The danger is the next code added to a
  `detach_isolated` future that calls `backend.stop(...)` — there
  is no compile-time guard.
- **R29-M1 (NEW MINOR)**: `futures::join!(t5_future,
  clock_resync_future)` at `wake_machine.rs:529-530` is
  cancel-safe under the production lifecycle (wake_machine runs
  in `detach_isolated`, no external cancellation surface). The
  brief asked for verification; the answer is YES,
  it's cancel-safe. Borrows (`&agent_url`,
  `&sealed.signing_key_bytes`) live for the whole join scope
  inside `drive()`; spawn_blocking work is owned by the OS thread
  and is documented as cancel-unsafe-but-completes (compio 0.11
  `runtime/mod.rs:208` "The task will not be cancelled even if
  the future is dropped"). Half-dead-agent fingerprint is
  structurally pinned via the `transport_error: bool` on each
  outcome rather than via string-matching the diagnostic
  message — concurrency-clean.
- **R29-M2 (NEW MINOR — observability gap)**: R29-C1's leak path
  emits the `stop_inner` "vm_index release" log line ONLY if the
  spawned task actually runs. On the snap-teardown short-lived
  runtime it doesn't, so the operator log stream shows NO release
  event for that sandbox_id — yet `stop_inner`'s upstream
  `tracing::info!(... "host_fence: cleared")` did fire. Operator
  scanning for "host_fence cleared without subsequent release" is
  a fingerprint for R29-C1 instances.
- **R29-M3 (NEW MINOR)**: the new
  `clock_resync_post_restore_typed` wrapper is a thin
  `match` over the existing `clock_resync_post_restore` Result.
  Transport-error detection via `message.starts_with("/_clock_resync
  transport:")` (line 3092) — string-coupled to the underlying
  function's error format. Today the only `format!("transport: {e}")`
  path in `clock_resync_post_restore` produces exactly this prefix
  (line 3028); the typed wrapper is a string-format dependency. A
  future refactor that changes the error string would silently
  break the half-dead-agent detector to "always false". Severity:
  MINOR (test
  `clock_resync_typed_surfaces_transport_error_on_closed_port`
  pins one direction; the inverse — that the prefix lines up — is
  pinned implicitly by the typed_non_200 test passing as not
  transport_error). Per zeroship "no back-compat" stance, the
  cleanest fix is a typed inner-helper return, not a string
  prefix; deferring to api-surface r29 owner call.
- **R29-CARRY**: all r28 carry items still open (R20-I2, R20-I3,
  R24-I1, R25-I1, R25-I2, R26-I1, R27-I1, R27-I2, R27-M1,
  R27-M2). No state changes this round.

## CRITICAL

### [R29-C1] (NEW CRITICAL) `snap-teardown-<tail>` detach_isolated runtime drops `spawn_delayed_release`'s 5 s timer — vm_index leak on every snapshot teardown

- **Files**:
  - `crates/sandbox/src/admin_handlers.rs:1486-1506` — the admin
    snapshot handler's detached teardown dispatch:
    ```rust
    let state_for_teardown = Arc::clone(&state);
    let sandbox_id_base62 = zeroship_core::typed_id::uuid_to_base62(&sandbox_id);
    let tail = sandbox_id_base62
        .get(sandbox_id_base62.len().saturating_sub(8)..)
        .unwrap_or(&sandbox_id_base62);
    crate::detach::detach_isolated(
        format!("snap-teardown-{tail}"),
        move || async move {
            if let Err(e) = state_for_teardown
                .backend
                .teardown_source_for_snapshot(sandbox_id)
                .await
            {
                tracing::error!(/* ... */);
            }
        },
    );
    ```
    The future body is ONLY the `.await` of
    `teardown_source_for_snapshot`. No surrounding loop. No
    follow-up await to extend the runtime lifetime past the
    release delay.
  - `crates/sandbox/src/backend/mod.rs:497-509` — dispatch:
    `Self::NomadCh(b) => b.stop_preserving_state(sandbox_id).await`.
  - `crates/sandbox/src/backend/nomad_ch.rs:1140-1145` —
    `stop_preserving_state` → `self.stop_inner(sandbox_id, false).await`.
  - `crates/sandbox/src/backend/nomad_ch.rs:1315-1332` — the
    `if fence_passed` branch calls
    `VmIndexAllocator::spawn_delayed_release(...)`.
  - `crates/sandbox/src/backend/nomad_ch.rs:363-387` — the
    helper: `compio::runtime::spawn(async move { sleep(delay).await;
    release(i); log; }).detach()`.
  - `crates/sandbox/src/detach.rs:76-110` — `detach_isolated`:
    `let rt = Runtime::new()?; let fut = make_fut(); rt.block_on(fut);`
    Runtime is a local binding — dropped at end of closure scope.
  - `target/.../compio-runtime-0.11.0/src/runtime/mod.rs:389-399`:
    ```rust
    impl Drop for Runtime {
        fn drop(&mut self) {
            if Rc::strong_count(&self.0) > 1 { return; }
            self.enter(|| { self.scheduler.clear(); })
        }
    }
    ```
  - `target/.../compio-runtime-0.11.0/src/runtime/scheduler/mod.rs:233-251`:
    `Scheduler::clear` "wakes up all active tasks ... then drops
    all scheduled tasks, which drops all futures and removes
    `Waker`s." The 5 s `compio::time::sleep` future is one of
    those dropped tasks.

- **The shape** (identical to R28-C1, sibling call site):

  ```
  T+0:      admin POST /admin/sandboxes/<id>/snapshot returns 200.
              admin_handlers.rs:1491 dispatches the detached teardown.
  T+0+ε:    detach_isolated spawns OS thread "snap-teardown-<tail>".
              That thread builds compio::runtime::Runtime, calls
              rt.block_on(teardown_future).
  T+~Xs:    teardown_future awaits stop_preserving_state →
              stop_inner(.., false). Steps 1-3 fire (shutdown probe,
              Nomad purge, wait_for_job_gone). On success+fence_passed,
              stop_inner calls VmIndexAllocator::spawn_delayed_release
              which does compio::runtime::spawn(async move {
              sleep(5s).await; release(i); log; }).detach() on the
              CURRENT runtime (the snap-teardown private runtime).
  T+~Xs+ε:  stop_inner returns Ok. stop_preserving_state returns Ok.
              teardown_future returns Ready.
  T+~Xs+ε:  block_on observes Ready, runs one more scheduler.run()
              cycle. The delayed-release future polls, registers a
              5 s timer with compio's timer driver, returns Pending.
  T+~Xs+ε:  block_on returns. `rt` drops at end of detach_isolated
              closure scope.
  T+~Xs+ε:  Runtime::Drop → scheduler.clear() → the pending delayed-
              release future is DROPPED. The 5 s sleep never fires.
              The release(i) never runs.
              vm_index is LEAKED.
  ```

- **Why r28's R28-C1 fix did NOT close this site**:

  The R28-C1 fix (`9e1f6276`) replaced the `spawn_delayed_release`
  call at `CreateGuard::drop` with an inline
  `compio::time::sleep(release_delay).await; release(i);`. That fix
  was scoped to the create-rollback site only. The `stop_inner`
  call site (`:1324`) was correctly identified as safe under its
  long-lived ntex-worker / `snap-idle-gc` runtime callers — but the
  `snap-teardown-<tail>` admin path ALSO calls `stop_inner` (via
  `stop_preserving_state`) and ALSO from a `detach_isolated`
  short-lived runtime. The r28 brief covered the `CreateGuard::drop`
  path but did not enumerate other `detach_isolated` callers of
  `stop` / `stop_preserving_state`. This site has been present
  since the r24-A2-S3 commit (`c969b94d`); the bug has been
  shipping for the last two cycles undetected.

- **Other `detach_isolated` callers — survey**:

  Enumerated `detach_isolated(...)` call sites and traced whether
  each can reach `spawn_delayed_release` via `stop_inner` /
  `stop_preserving_state`:

  | call site | runtime lifetime | calls `stop` / teardown? | risk |
  |---|---|---|---|
  | `lib.rs:1427` `snap-health` | long-lived `loop { sleep(30s) … probe() }` | no | clean |
  | `lib.rs:1514` `snap-heartbeat` | long-lived `loop { sleep(5s) … heartbeat() }` | no | clean |
  | `lib.rs:1731` `snap-takeover` | long-lived `loop { sleep(30s) … rehydrate_after_takeover() }` | no | clean (no `backend.stop` reached) |
  | `sweep.rs:256` `snap-transient` | long-lived `loop { sleep(...) … run_transient_takeover_once() }` | no | clean |
  | `sweep.rs:335` `wake-gc` | long-lived `loop { sleep(...) … gc_expired_wake_jobs() }` | no | clean |
  | `sweep.rs:441` `wake-takeover` | long-lived `loop { sleep(...) … run_wake_jobs_takeover_once() }` | no | clean (no `backend.stop` reached) |
  | `sweep.rs:780` `snap-idle-evict` | long-lived `loop { sleep(...) … run_idle_eviction_once() }` which calls `teardown_source_for_snapshot` → `stop_inner` → `spawn_delayed_release` | yes | **clean** (runtime re-sleeps `interval` after each tick — 5 s task fires before next sleep returns) |
  | `sweep.rs:1246` `host-dir-gc` | long-lived `loop { sleep(...) … host_dir_gc() }` | no | clean |
  | `registry.rs:836` `snap-idle-gc` | long-lived `loop { sleep(60s) … backend.stop(id) }` (sees R28-C1 r28 review for sign-off) | yes via `backend.stop` → `stop_inner` → `spawn_delayed_release` | **clean** (5 s fits inside 60 s sleep) |
  | `snapshot_store_gcs.rs:1143` `snap-l2-upload-<tail>` | short-lived (one-shot `l2.put(...)`) | no — sync put, no `backend.stop` | clean |
  | `admin_handlers.rs:1491` `snap-teardown-<tail>` | **SHORT-LIVED** (one-shot `teardown_source_for_snapshot.await`) | yes via `teardown_source_for_snapshot` → `stop_preserving_state` → `stop_inner` → `spawn_delayed_release` | **BROKEN — R29-C1** |
  | `admin_handlers.rs:1851` `wake-<tail>` | wake_machine `drive()` — uses `teardown_restore` which is SYNC `release_vm_index` (no spawn_delayed_release) | indirect rollback only | clean |
  | `backend/nomad_ch.rs:2215` `create-rollbk` | **SHORT-LIVED** (one-shot cleanup_future) | inlined sleep+release post-R28-C1 fix | clean (R28-C1 closed) |

  Result: ONE NEW broken site (`snap-teardown-<tail>`), one
  already-closed site (`create-rollbk`), plus a structural
  warning — see R29-I1 for the helper-still-a-footgun finding.

- **Severity**: CRITICAL. Every admin-triggered snapshot
  (`POST /admin/sandboxes/{id}/snapshot`) ends in a
  `snap-teardown-<tail>` detached teardown. If the teardown reaches
  the `fence_passed` branch (i.e., the normal happy-path success
  case), the spawn_delayed_release task is planted and dropped.
  **Every successful snapshot leaks its vm_index** until the next
  controller boot's orphan-prune. The idle-eviction sweep path
  (`snap-idle-evict`) is fine because its runtime re-sleeps
  `SANDBOX_IDLE_SNAPSHOT_SWEEP_SECS` between iterations, but the
  admin snapshot path leaks every time.

- **Failure shape in production**:

  - On every successful admin snapshot, the vm_index moves from
    "in-use" to "released-then-dropped-by-runtime-clear" instead
    of "released back into freed set."
  - The slot stays in `next` allocator state (handed out, not
    in `freed`). Next CREATE for any user gets `next+1`; the
    leaked slot is unreachable until controller restart.
  - With sustained admin snapshot traffic, pool exhaustion is
    `(ceil - floor + 1) / snapshot_rate` until next controller
    boot.
  - Critically, this is the same leak shape the r28 review
    identified for failed-create — but here it's on the SUCCESS
    path. The leak rate equals the admin snapshot success rate,
    which in stress-r9-style workloads can be 100s/cycle.

- **Why it's not caught by existing tests**:

  - `restored_sandbox_is_stoppable_and_releases_vm_index` (the
    pg-e2e fixture) uses `vm_index_release_delay_secs: 0`, which
    means `spawn_delayed_release` collapses the spawn+sleep to a
    near-synchronous body that completes within one
    post-Ready `self.run()` cycle. So the leak path is invisible
    under delay==0.
  - `spawn_delayed_release_honors_configured_delay` (unit test at
    line 4741) uses `delay=200ms` but its outer test future keeps
    polling `compio::time::sleep(...)` for `delay + 500ms`. The
    test runtime stays alive for the full window. Production has
    NO such outer poll loop on the snap-teardown path.
  - The new R28-C1 oracle test
    (`create_guard_drop_releases_vm_index_under_isolated_runtime`
    added in `9e1f6276`) exercises CreateGuard::drop ONLY. There
    is NO equivalent oracle for the snap-teardown admin handler.

- **Recommendations** (no implementation; reviewer-only):

  1. **Apply the same Option A fix** the R28-C1 closed at
     CreateGuard::drop — inline `compio::time::sleep(...).await;
     release(i)` inside `stop_inner` at line 1324. But this would
     change the long-lived-runtime callers' semantics too
     (snap-idle-gc / ntex-worker HTTP stop) — they'd now wait
     5 s before `stop()` returns, blocking the HTTP handler.
     That's a regression on the synchronous admin DELETE path
     (`handlers.rs:667`).

  2. **Better** — leave `stop_inner`'s `spawn_delayed_release`
     for long-lived runtimes; change the
     `snap-teardown-<tail>` admin handler's future to NOT use
     `detach_isolated` for the short-lived case. Either:
     - inline the spawn_delayed_release equivalent in the admin
       handler future after `teardown_source_for_snapshot`
       returns, OR
     - extend the admin future's lifetime by awaiting an
       explicit `sleep(release_delay)` after the teardown returns
       (i.e., make the snap-teardown thread live for the
       teardown duration + release_delay).

  3. **Best (structural)** — return a `Task` JoinHandle from
     `spawn_delayed_release` instead of `.detach()`-ing.
     Callers in short-lived runtimes await the Task before
     returning; long-lived callers can either await or detach
     explicitly. This makes the runtime-lifetime invariant
     EXPLICIT in the type system. The R28-C1 r28 review listed
     this as Option C. The architecture-r29 r29-A2
     class-fix recommendation aligns with this.

  4. **Test oracle** (parallel to R28-C1's test): a unit test
     that dispatches a fake `teardown_source_for_snapshot`
     through `detach_isolated("snap-teardown-fixture", ...)` with
     `release_delay > 0` and asserts the vm_index reappears in
     the allocator post-OS-thread-join. ~40 LOC, same shape as
     the R28-C1 fixture in `9e1f6276`. The fixture MUST NOT keep
     a polling loop alive on a SHARED runtime — it must use
     `detach_isolated` (or equivalent) and observe the allocator
     state AFTER the OS thread joins.

- **Cross-lens**:
  - **test-coverage r29**: missing oracle for `snap-teardown-<tail>`
    same shape as R28-C1's. The fact that R28-C1's oracle did NOT
    catch this sibling site is itself a test-coverage finding —
    the oracle should have been generalized to cover ANY
    detach_isolated → spawn_delayed_release path, not just
    CreateGuard::drop.
  - **arch-r29 r29-A2**: structurally confirmed — the helper IS a
    footgun. The architecture-r29 class-fix recommendation
    (return a Task) is the right move; concurrency-r29 endorses.
  - **observability-r29**: R29-M2 below — the operator-visible
    fingerprint of R29-C1 is `host_fence: cleared` log line
    WITHOUT a subsequent `vm_index released` log line for that
    sandbox_id. Worth a runbook entry until the bug closes.
  - **stress-r9 cluster**: any stress run that exercises the
    admin snapshot path will leak slots on every snapshot. The
    stress-r9 scripts are currently stuck on a different issue
    (driver loading); once they're unstuck and start hitting the
    happy path, R29-C1 will manifest as "vm_index exhausted"
    after `(ceil - floor + 1)` successful snapshots.

- **Carry mechanism**: NEW for r29. CRITICAL.

## IMPORTANT

### [R29-I1] (NEW IMPORTANT, observational) `spawn_delayed_release` remains a public footgun; r29-A2 class-fix endorsed

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:363-387` —
    `pub fn spawn_delayed_release(...)`. No visibility downgrade
    despite R28-C1's lesson.
  - `crates/sandbox/src/backend/nomad_ch.rs:2275-2301` — the
    R28-C1 fix's rustdoc on the inline path warns against
    unifying the two call sites by changing the helper. That
    warning protects the EXISTING two sites but does nothing
    for the NEW (third) site at `admin_handlers.rs:1491` that
    silently reaches the helper via the stop_inner call graph.

- **The shape**:

  `spawn_delayed_release`'s contract — "fire-and-forget delayed
  release on the current compio runtime" — is **only safe if the
  current runtime is long-lived past the delay**. Today three call
  sites reach the helper:

  1. `stop_inner:1324` — DIRECT call. Safe when called from
     long-lived runtimes (ntex worker for HTTP stop;
     snap-idle-gc for the registry sweep). UNSAFE when called
     from `snap-teardown-<tail>` (R29-C1) — but the caller
     doesn't see it because the call is buried in `stop_inner`.
  2. `CreateGuard::drop:2272` (historical) — INLINED post-R28-C1.
     Safe.
  3. Unit tests at `nomad_ch.rs:4714 / :4747` — safe by test
     fixture design (outer poll loop keeps the test runtime
     alive).

  The danger: a future code change that wraps
  `backend.stop(...)` or `backend.teardown_source_for_snapshot(...)`
  in ANY new `detach_isolated`-style short-lived runtime will
  reproduce R28-C1 / R29-C1 with no compile-time signal.

- **Concurrency analysis — why no compile-time signal**:

  1. `spawn_delayed_release` is `pub` (no visibility downgrade in
     r29). Callers see the name and inferred contract: "delayed
     release; fire and forget."
  2. The function `.detach()`'s the `Task` — caller never gets a
     JoinHandle, so there's no "must await this to ensure
     completion" hint.
  3. The "current compio runtime" implicit lifetime is invisible
     at the call site — caller has no way to know whether
     stop_inner runs on a long-lived or short-lived runtime
     without reading the entire call graph.
  4. The R28-C1 fix's rustdoc lives at the
     CreateGuard::drop INLINE site, NOT at the helper's
     declaration. A new caller reading the helper sees no
     warning.

- **Severity**: IMPORTANT (structural — the bug class will recur).
  R29-C1 above is the IMMEDIATE manifestation; R29-I1 is the
  CLASS-OF-BUG observation.

- **Recommendations** (no implementation; reviewer-only):

  1. **Endorse arch-r29 r29-A2 class-fix**: change
     `spawn_delayed_release` to return a `Task`; remove
     `.detach()` from the helper. Caller MUST decide: await
     (short-lived runtime) or `.detach()` (long-lived runtime).
     The type system now forces the runtime-lifetime decision
     at every call site.

  2. **Alternative**: rename to
     `spawn_delayed_release_on_long_lived_runtime` and document
     the runtime-lifetime invariant in the helper's rustdoc.
     Cheaper than (1) but doesn't enforce the invariant at the
     type level.

  3. **Alternative**: introduce an explicit `LongLivedRuntime`
     ZST type and have the helper require `&LongLivedRuntime` as
     a witness — call sites in `detach_isolated` futures cannot
     construct one. Most type-safe; heaviest refactor.

- **Cross-lens**: arch-r29 owns the helper-API decision;
  concurrency-r29 endorses any of options 1-3 above. Option 1 is
  the lowest churn.

- **Carry mechanism**: NEW for r29; CLOSES with arch-r29-A2
  class-fix.

## MINOR

### [R29-M1] (NEW MINOR) `futures::join!(t5_future, clock_resync_future)` cancel-safety: VERIFIED

- **Files**:
  - `crates/sandbox/src/wake_machine.rs:518-530` — the
    `futures::join!` call site.
  - `crates/sandbox/src/restore_handler.rs:2947-3032` —
    `clock_resync_post_restore`'s `spawn_blocking` body.
  - `crates/sandbox/src/restore_handler.rs:3231-3289` —
    `verify_agent_version_post_restore`'s `spawn_blocking` body.

- **The shape**:

  Round-29 brief flagged this as a check item; concurrency-r29
  CONFIRMS cancel-safety per the following analysis.

  Both futures wrap `compio::runtime::spawn_blocking` over a
  ureq call. The futures take all arguments by value (`SigningKey`
  is built from the borrowed bytes via
  `SigningKey::from_bytes(signing_key_bytes)` BEFORE the
  spawn_blocking move). The `url_for_blocking = url.clone()`
  pattern (line 2958, 3253) and the JSON body / nonce / signature
  construction all happen on the calling thread before the
  spawn_blocking moves OWNED data into the OS thread.

  Borrows held across the join (`&agent_url`,
  `&sealed.signing_key_bytes`): these live INSIDE the
  `verify_agent_version_post_restore` / `clock_resync_post_restore_typed`
  futures' captured state. The borrows are valid for the duration
  of `drive()`'s scope, which lives well past the join. No
  cross-await borrow leak.

  Cancel-safety property (compio 0.11 `runtime/mod.rs:208`):
  spawn_blocking is documented as "task will not be cancelled
  even if the future is dropped." So if the outer task were
  cancelled mid-join, both spawn_blocking OS threads continue to
  completion; their results are dropped via the discarded
  JoinHandle. Read-only HTTP calls (`GET /version`,
  `POST /_clock_resync`) — discarded results are inert.

  Critically, the wake_machine runs in `detach_isolated`, which
  has NO external cancellation surface in production. The
  `drive()` future is `block_on`'d on a dedicated OS thread; the
  only way it ends is by completing normally. So the
  cancel-safety property is observed-but-not-load-bearing.

- **Severity**: MINOR (verification of a check item).

- **Cross-lens**: none.

- **Carry mechanism**: NEW for r29 (verifies r28 follow-up).

### [R29-M2] (NEW MINOR — observability gap from R29-C1) `host_fence: cleared` WITHOUT subsequent `vm_index released` is the fingerprint

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:1287-1291` — the
    `host_fence: cleared` log line that fires BEFORE the
    `spawn_delayed_release` call.
  - `crates/sandbox/src/backend/nomad_ch.rs:378-384` — the
    `vm_index released (r24-A2-S3 delayed)` log line INSIDE the
    spawned future.

- **The shape**:

  R29-C1's leak path: `stop_inner` logs `host_fence: cleared`
  (line 1287-1291) synchronously, THEN calls
  `spawn_delayed_release`. The spawned future's log line at
  3078-3084 is what tells operators the release actually fired.
  On the snap-teardown short-lived runtime path, the future is
  dropped before its body runs — so the operator log stream
  shows:
  - `host_fence: cleared` for sandbox X (line 1287-1291)
  - NO subsequent `vm_index released` for sandbox X

  Anyone scanning the log stream for "did the release actually
  fire after fence cleared?" will see the gap.

- **Severity**: MINOR (observability follow-on of R29-C1).
  Fixing R29-C1 (per the recommendations above) closes this
  automatically.

- **Cross-lens**: observability-r29 owns the runbook entry
  proposal — operators with a stress-r9-style workload should be
  alerted to "fence_cleared events without subsequent
  release events" as a pre-launch leak fingerprint. Until
  R29-C1 closes, this is the load-bearing dashboard signal.

- **Carry mechanism**: NEW for r29; closes with R29-C1.

### [R29-M3] (NEW MINOR) `clock_resync_post_restore_typed` transport_error detection is string-coupled to the underlying error format

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:3076-3099` —
    `clock_resync_post_restore_typed` wrapper. Returns
    `transport_error: true` iff
    `message.starts_with("/_clock_resync transport:")`.
  - `crates/sandbox/src/restore_handler.rs:3028` —
    `Err(e) => Err(format!("/_clock_resync transport: {e}"))`
    in the underlying `clock_resync_post_restore`.

- **The shape**:

  The half-dead-agent fingerprint (R28-I2 → R29 follow-up) is
  load-bearing on the typed wrapper's `transport_error: bool`.
  That boolean is derived by string-prefix-matching the inner
  function's error message. Today the prefix
  `/_clock_resync transport: ` is the ONLY format that the
  ureq fall-through error path emits (line 3028). All other
  failure modes (status code from `Err(ureq::Error::Status)` at
  line 3021, panic at line 3032) produce different prefixes.
  So the boolean is correct today.

  But: it's a string-format dependency. A future refactor of
  `clock_resync_post_restore`'s error message format
  (e.g., adding a request-ID prefix, switching from `format!` to
  `anyhow!`-style structured errors, lower-casing the verb,
  renaming the path) would silently flip the boolean to "always
  false" — and the half-dead-agent detector would stop firing.
  This is the kind of latent breakage that survives a quick
  visual review.

  The test
  `clock_resync_typed_surfaces_transport_error_on_closed_port`
  (added in `d00f12dd`) pins one direction (closed port →
  transport_error=true). The inverse — that the underlying
  function still produces the matching prefix when the
  transport-error path fires — is pinned IMPLICITLY by the
  `clock_resync_typed_non_200_is_not_transport_error` test
  passing as `transport_error=false`. But a future refactor that
  changes the prefix WHILE preserving the closed-port behaviour
  semantically (e.g., still failing the spawn_blocking but with
  a different error string) would silently break the contract.

- **Severity**: MINOR (string-format coupling; survives current
  tests but fragile under refactor). Concurrency-clean per se —
  no race, no shared-state issue. Flagged as a code-quality
  hazard adjacent to the structured-boolean contract claim in
  the commit message ("the bool is the routing contract").

- **Recommendations** (no implementation; reviewer-only):

  1. Per zeroship "no back-compat" stance: change
     `clock_resync_post_restore` to return a typed error enum
     instead of `Result<(), String>`. The typed wrapper consumes
     the enum variant directly; no string-prefix match. The sync
     path's `Result<(), String>` shape is preserved by `.map_err`-ing
     in the public function. ~30 LOC.

  2. Alternative: keep the string-prefix match but assert the
     prefix at compile time via a `const PREFIX: &str = ...;`
     shared between the producer (line 3028) and the consumer
     (line 3092). Cheaper, less robust.

  3. Alternative: a dedicated test
     `clock_resync_typed_prefix_matches_underlying_function`
     that mints a transport error in the underlying function
     and asserts the typed wrapper sees `transport_error=true`
     end-to-end. Already implicitly covered, but making it
     explicit hardens the contract.

- **Cross-lens**: api-surface r29 / code-quality r29 own the
  refactor-or-document call. Concurrency-r29 raises the
  fragility but has no race finding.

- **Carry mechanism**: NEW for r29.

## Items NOT findings (verified clean this round)

### [N/A] R28-C1 fix at `CreateGuard::drop` — VERIFIED CLOSED

`crates/sandbox/src/backend/nomad_ch.rs:2302-2315`. Inline
`if !release_delay.is_zero() { compio::time::sleep(release_delay).await; }
vm_index_allocator.lock().release(i); tracing::info!(...)`. The
cleanup future now lives for the release_delay duration; the
short-lived `create-rollbk` runtime stays alive until the inline
sleep completes. The rustdoc at lines 2275-2301 explains why this
site differs from stop_inner and warns against unifying the call
sites by changing the helper. Concurrency-clean.

### [N/A] R28-I1 fix — `futures::join!(t5, clock_resync)` parallelisation

`crates/sandbox/src/wake_machine.rs:518-530`. See R29-M1 for the
cancel-safety verification. The parallel arity is correctly
pinned by the new test
`parallel_t5_and_clock_resync_both_succeed_against_healthy_agent`.
Concurrency-clean.

### [N/A] R28-I2 fix — structured `transport_error: bool` + half-dead-agent fingerprint

`crates/sandbox/src/wake_machine.rs:539-574`. The
`half_dead_agent` paired-fail check fires BEFORE branching on
the individual outcomes; ordering is correct (mismatched-build
takes precedence over transport-error per the wake-machine
contract). The rollback `WakeErrorCode::ClockResyncFailed`
carries `"half_dead_agent:"` prefix in the message for log
attribution. See R29-M3 for the string-coupling caveat on the
wrapper itself; the wake_machine-side consumption is
structurally clean.

### [N/A] `spawn_delayed_release` at `stop_inner` long-lived call sites

`crates/sandbox/src/backend/nomad_ch.rs:1324`. Reached from
`handlers.rs:667` (HTTP stop, ntex worker runtime — long-lived)
and `registry.rs:868` (snap-idle-gc loop, runtime re-sleeps
60 s after each iteration). The 5 s delayed-release task DOES
fire in both call sites. Verified clean per the r28 review.

### [N/A] `snap-idle-evict` loop's `teardown_source_for_snapshot` call

`crates/sandbox/src/sweep.rs:585-597`. The sweep loop calls
`state.backend.teardown_source_for_snapshot(sandbox_id).await`
WITHIN its long-lived `loop { sleep(interval) … }` runtime. The
runtime stays alive past the 5 s release delay (interval ≥ 1 s
floor; default `SANDBOX_IDLE_SNAPSHOT_SWEEP_SECS` is much
larger). The 5 s delayed-release task fires before the loop's
next sleep returns. Verified clean.

## Round-briefing answers (summary)

### 1. `futures::join!(t5_future, clock_resync_future)` cancel-safety: VERIFIED

YES — cancel-safe. See R29-M1. The futures take all bytes by
value at the spawn_blocking boundary; borrows held across the
join (`&agent_url`, `&sealed.signing_key_bytes`) live for the
duration of `drive()`'s scope. spawn_blocking is documented
cancel-unsafe-but-completes (compio 0.11 `runtime/mod.rs:208`);
the wake_machine has no external cancellation surface in
production anyway. Half-dead-agent fingerprint is structurally
pinned via `transport_error: bool` on each outcome.

### 2. r24-A2-S3 VmIndexAllocator delay — `stop_inner:1324` still correctly using `spawn_delayed_release` on long-lived runtime?

The DIRECT call sites of `stop_inner` are long-lived
(`handlers.rs:667` ntex worker; `registry.rs:868` snap-idle-gc
re-sleep loop). The INDIRECT call site `snap-idle-evict`
(`sweep.rs:585`) goes through `teardown_source_for_snapshot` →
`stop_preserving_state` → `stop_inner` on its long-lived sweep
loop runtime. All clean.

**But** there is ONE call site where `stop_inner` is reached
from a SHORT-LIVED `detach_isolated` runtime: the admin
snapshot handler at `admin_handlers.rs:1491` → snap-teardown-<tail>.
This is R29-C1 (NEW CRITICAL). Same shape as R28-C1, sibling
call site, NOT closed by the R28-C1 inline-sleep fix.

### 3. r29-A2 — is there ANOTHER detach_isolated call site currently spawning a delayed task that could silently drop?

YES. **One: the admin snapshot handler's `snap-teardown-<tail>`
detach_isolated future.** See R29-C1. Surveyed all 12
`detach_isolated` call sites in the production sandbox crate;
exactly one matches the failure shape (short-lived runtime +
reaches `spawn_delayed_release` via the call graph).

Class-fix recommendation (architecture-r29-A2): change
`spawn_delayed_release` to return a `Task` JoinHandle instead
of `.detach()`-ing. Endorsed. The current helper is a structural
footgun (R29-I1) — even if R29-C1 closes by patching the admin
handler, the third (and fourth, fifth, …) detach_isolated
caller of `backend.stop(...)` will silently reproduce the same
bug. Type-system-enforced lifetime is the right answer.

## Cross-lens consensus

- **arch-r29 owns**: (1) R29-I1 helper-API class-fix
  (r29-A2 class-fix is the right move; concurrency-r29 endorses);
  (2) the carry items unchanged (R27-I1, R27-I2, R26-I1, R25-I2,
  R20-I2/I3, R24-I1).
- **test-coverage r29 owns**: (1) R29-C1 oracle — ~40 LOC test
  asserting `snap-teardown-<tail>` short-lived runtime
  reproduces the leak shape until fixed; (2) the meta-finding
  that R28-C1's oracle should have been generalised to cover
  any detach_isolated → spawn_delayed_release path; (3) R25-I1
  carry.
- **perf-r29 owns**: R28-DEF1 idle-conn-count interaction
  carry; R26-C1 thread-local-pool teardown cost carry.
- **observability-r29 owns**: (1) R29-M2 fingerprint runbook
  entry (`host_fence: cleared` without subsequent
  `vm_index released` is the R29-C1 fingerprint); (2) the
  R28-I2 metric proposal for half-dead-agent
  (`sandbox_wake_agent_half_dead_total`) — not added yet,
  carry.
- **code-quality / api-surface r29 owns**: R29-M3
  string-coupling fragility; R27-M1 host_dir_created
  doc-comment carry; R24-I1 fsync_dir doc-misframe carry.
- **stress-r9 cluster**: the cluster scripts are stuck on a
  driver-load issue (per def11cb4 / baf1b78c / 0bbdae94). Once
  unstuck and hitting the happy path, R29-C1 will manifest as
  "vm_index exhausted" after roughly
  `(ceil - floor + 1)` successful admin snapshots — same
  failure-mode prediction the r28 brief made for the failed-
  create path, now applicable to the admin-snapshot success
  path. Operators should expect the symptom on the success
  side, not just the failure side.

## Carry table

| Finding | Source | r29 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry |
| R20-C1 (Failed side) | r20 → CLOSED r22+r23 e2e | fully CLOSED |
| R20-C1 / R25-I2 (Ok side) | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| R20-I2 sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 11th cycle** |
| R20-I3 / R21-I1 / R24-M2 Restoring watchdog | r20→r28 | **STILL OPEN — 11th cycle** (promote IF stress-r9 cold-cache stretches >60s) |
| R22-I1 observability-gap | r22 → CLOSED r23 | CLOSED carry |
| R22-M3 cross-controller claim | r22 minor | carry |
| R23-I1 (Failed side) | r23 → CLOSED r24 | CLOSED carry |
| R24-I1 fsync_dir doc-misframe | r24 NEW-IMP | **STILL OPEN — code-quality r29 owns** |
| R24-M1 sweep × machine race-into-terminal | r24 minor | carry |
| R24-M3 stranded taps (C-N-W2) | r24 minor | carry |
| R25-I1 sweeper TOCTOU invariant untested | r25 NEW-IMP | **STILL OPEN** (Phase 2 does NOT obsolete; carry) |
| R25-I2 Phase::Ok terminal-overwrite divergence | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R25-M1 typed StagingPathMissing carry-through | r25 minor | carry (doc-comment ask) |
| R26-I1 boot-lookup serial-await + external-dep | r26 NEW-IMP | **STILL OPEN — PRIORITY HOLDS, reassess at Phase 5** |
| R26-M1 from_config_full threading consistent | r26 minor | CLOSED-with-watchful-eye |
| R26-M2 R25-T4 helper extraction race-clean | r26 minor | CLOSED-with-watchful-eye |
| R26-M3 read_snapshot_row DRY (concurrency angle) | r26 minor | carry (code-quality owns) |
| R26-DEF1 perf-r25 C1 concurrency recheck | r26 defer | CLOSED-with-watchful-eye |
| R27-I1 host_dir mtime baseline shifts | r27 NEW-IMP | **STILL OPEN — arch-r29 owns** |
| R27-I2 home.img per-user race + Phase 5 latent | r27 NEW-IMP | **STILL OPEN — arch-r29 owns** |
| R27-M1 host_dir_created semantic-overload | r27 minor | carry |
| R27-M2 wait_for_alloc_running budget shift | r27 minor | carry (operator-doc hygiene) |
| R27-M3 R25-I1 sweeper TOCTOU shape SHIFTS | r27 minor | confirmation (no change) |
| R27-DEF1 R26-C1 thread-local Rc<Pool> | r27 defer | CLOSED-with-watchful-eye |
| R28-C1 spawn_delayed_release lost on CreateGuard::drop | r28 NEW-CRIT | **CLOSED at `9e1f6276`** (Option A inline-sleep) |
| R28-I1 T5 + clock_resync SERIAL; not parallelised | r28 NEW-IMP | **CLOSED at `d00f12dd`** (futures::join!) |
| R28-I2 T5 Skipped{transport_error} → 2x failure latency | r28 NEW-IMP | **CLOSED at `d00f12dd`** (structured transport_error: bool + half-dead-agent detector) |
| R28-M1 verify_agent_version cancel-unsafe by convention | r28 minor | carry (hygiene; no production cancel point) |
| R28-M2 missing log line on CreateGuard::drop release | r28 minor | CLOSED with R28-C1 |
| R28-M3 start_housekeeper concurrency-recheck | r28 minor | CLOSED-with-watchful-eye |
| R28-DEF1 idle-conn count under 500-max-conns | r28 defer | carry (perf-r29 owns) |
| **R29-C1** spawn_delayed_release lost on snap-teardown-<tail> | r29 NEW-CRIT | NEW |
| **R29-I1** spawn_delayed_release helper is a public footgun | r29 NEW-IMP | NEW (endorse arch-r29-A2 class-fix) |
| R29-M1 futures::join! cancel-safety VERIFIED | r29 minor | NEW (verification only) |
| R29-M2 host_fence: cleared without subsequent vm_index released | r29 minor | NEW (closes with R29-C1) |
| R29-M3 typed transport_error string-coupling | r29 minor | NEW (api-surface r29 / code-quality r29 own) |
| Older minors (R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1, R23-M1, R24-M5, R25-M2, R25-M3) | older minor | carry |

## Status block

```
Round 29 (R28-C1 / R28-I1 / R28-I2 CLOSED; R29-A2 class-fix
          ENDORSED; sibling site R29-C1 NEW CRITICAL;
          stress-r9 cluster still stuck on driver-load):

  CLOSED THIS ROUND:
    R28-C1 spawn_delayed_release dropped at CreateGuard::drop
      — CLOSED at 9e1f6276 (Option A inline-sleep+release).
      Verified: rustdoc at lines 2275-2301 explains lifetime;
      new test at line 4597 (per commit message) pins
      non-zero-delay under isolated runtime.
    R28-I1 T5 + clock_resync serial → parallel
      — CLOSED at d00f12dd (futures::join!).
      Verified cancel-safe (R29-M1).
    R28-I2 transport_error structured boolean +
      half-dead-agent detector
      — CLOSED at d00f12dd (typed VersionCheckOutcome::Skipped
      + ClockResyncOutcome::Err with transport_error: bool).

  NEW CRITICAL:
    R29-C1 spawn_delayed_release dropped at snap-teardown-<tail>
      detach_isolated short-lived runtime — vm_index leak on
      EVERY successful admin snapshot. Same shape as R28-C1,
      sibling call site, NOT closed by the R28-C1 inline-sleep
      fix (which scoped to CreateGuard::drop only).

  NEW IMPORTANT:
    R29-I1 spawn_delayed_release remains a public footgun;
      structural class-bug recurrence inevitable until the
      arch-r29-A2 class-fix (return Task, not .detach()) lands.
      Endorse arch-r29 r29-A2.

  NEW MINOR:
    R29-M1 futures::join! cancel-safety VERIFIED (no defect;
      verification only).
    R29-M2 host_fence: cleared without subsequent vm_index
      released is the R29-C1 fingerprint (observability
      follow-on of R29-C1).
    R29-M3 clock_resync_post_restore_typed transport_error
      detection string-coupled to underlying error format
      (fragile under refactor; structurally clean today).

  STILL OPEN (carry):
    R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1,
    R27-I1, R27-I2, R27-M1, R27-M2.

  ASK:
    (1) test-coverage r29: oracle for R29-C1 — ~40 LOC test
        asserting snap-teardown-<tail> short-lived runtime
        reproduces the leak shape with non-zero
        release_delay. Same pattern as R28-C1's oracle in
        9e1f6276 but for the admin snapshot path. Plus a
        META-finding: the R28-C1 oracle should have been
        generalised to cover ANY detach_isolated →
        spawn_delayed_release path, not just CreateGuard::drop.
    (2) arch r29: R29-C1 + R29-I1 class-fix design call.
        Concurrency-r29 endorses arch-r29-A2 (change helper
        to return Task; remove .detach()). The R29-C1
        admin-handler patch can be folded into the same PR.
    (3) arch r29 (carry from r28): R28-I2 half-dead-agent
        metric proposal — should
        sandbox_wake_agent_half_dead_total be added now or
        deferred?
    (4) arch r29 (carry): R27-I1 doc-comment vs rename for
        host_dir_created.
    (5) arch r29 (carry): R27-I2 home.img per-user race +
        Phase 5 latent.
    (6) arch r29 (carry): R26-I1 boot-lookup serial-await.
    (7) arch r29 (carry): R25-I2 STILL OPEN-PRIORITY-FALLS.
    (8) code-quality / api-surface r29: R29-M3 typed transport
        error string-coupling — refactor (typed inner-helper
        return) or document?
    (9) observability r29: R29-M2 fingerprint runbook entry;
        R28-I2 metric proposal carry.
    (10) perf r29: R28-DEF1 idle-conn interaction carry.
    (11) cluster T-8b-stress-r9: scripts still stuck on driver
         load. Once unstuck and hitting the happy admin
         snapshot path, R29-C1 will manifest as "vm_index
         exhausted" after (ceil-floor+1) successful admin
         snapshots. Operators should expect the symptom on
         the SUCCESS side (admin snapshot), not just on the
         failure side (R28-C1 failed-create).
```

## ASK clarifications for the user

Four open design questions concurrency-r29 raises that need owner
calls before r30:

1. **R29-C1 + R29-I1 disposition**: concurrency-r29 strongly
   recommends folding the snap-teardown-<tail> fix into the
   arch-r29-A2 class-fix PR. Change `spawn_delayed_release` to
   return `Task` (no `.detach()`); update both production call
   sites (stop_inner direct, plus the admin snapshot handler's
   detach_isolated future) to either `.detach()` explicitly
   (long-lived) or `.await` (short-lived). The R28-C1 inline
   approach at CreateGuard::drop can stay as-is (it's already
   correct and the rustdoc explains why) OR be refactored to use
   the new Task-returning helper for consistency — concurrency-r29
   leans toward refactoring for uniformity.

2. **R29-M3 disposition**: change
   `clock_resync_post_restore`'s `Result<(), String>` to a typed
   error enum (zeroship "no back-compat" stance: just do it,
   ~30 LOC), OR keep the string-prefix match but pin the prefix
   with a shared `const`. Concurrency-r29 mildly prefers the
   typed-error approach because the half-dead-agent detector is
   load-bearing on the boolean — fragility under refactor is
   real.

3. **Carry items**: R27-I1 (host_dir mtime + observability),
   R27-I2 (home.img per-user under Phase 5), R26-I1 (boot
   lookup), R20-I2 (sweep host-scoping), R20-I3 (Restoring
   watchdog) — all unchanged this round. arch-r29 owns; user
   confirm whether to fold these into a Phase 5 disposition
   doc or leave as carry through r30.

4. **Stress-r9 retry cadence**: the cluster scripts have been
   stuck on the driver-load + heredoc issues for multiple
   cycles. Concurrency-r29's R29-C1 prediction (admin snapshot
   path leaks every success) can only be validated once
   stress-r9 hits the happy path. Suggest deprioritising the
   stress-r9 retries until R29-C1 (or the arch-r29-A2 class-fix)
   lands — otherwise stress-r9 will surface "vm_index exhausted"
   as a new symptom AFTER spending hours chasing the driver-load
   issue, and the operator dashboard signal will be ambiguous.
