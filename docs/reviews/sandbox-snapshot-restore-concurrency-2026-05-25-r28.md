# Sandbox/snapshot-restore — concurrency r28 review

Date: 2026-05-25 (UTC).
HEAD at audit: `5a0647c3` (worktree
`.worktrees/sandbox-snapshot-restore`, branch `master`).
Round 28 of N. READ-ONLY.

Scope since r27 (`054bd9ef` → `5a0647c3`, sandbox-crate only):

- **r24-A2-S3 VmIndexAllocator delayed release** (`c969b94d`) — 5s
  defense-in-depth delay between Nomad stop ACK and slot return to
  the allocator, spawned as detached compio task. Two call sites
  converted: `stop_inner` (long-lived ntex worker / `snap-idle-gc`
  isolated runtime) and `CreateGuard::drop` (short-lived
  `detach_isolated("create-rollbk", ...)` runtime).
- **T5 agent /version verification** (`035c3564` + supporting
  `ca8d960a` + `ce218860`) — new probe inserted between unseal and
  `clock_resync_post_restore` in `wake_machine.rs::run`. Reuses the
  per-sandbox signing key. Mismatch rolls back with
  `WakeErrorCode::AgentVersionMismatch` (migration 0014).
- **R26-C1 thread-local Rc<Pool>** follow-ups: `start_housekeeper`
  on both pool roles (`8c0b361e`) + pg-gated cache predicate tests
  (`871752c7`).
- **R4-S2** `ErrorEnvelope::with_extra` type-tightening
  (`425a5522`) — synchronous helper; no concurrency surface.
- **scripts/stress-r7-A pg bump** (`e3291b62`),
  **T-8b-driver-v19 pin** (`086971d2`),
  **virtio-blk fixture refresh** (`579369bb`) — out-of-scope
  (scripts + test fixtures only).

In flight: stress-r7 cluster cycle (scripts-only, per briefing).

Prior: `…concurrency-2026-05-25-r27.md`.

## Summary

- **9 findings** this round (**1 NEW CRITICAL**, 2 new IMPORTANT,
  3 new MINOR, 1 cross-lens DEFER, 2 carry confirmations).
- **R28-C1 (NEW CRITICAL)**: `VmIndexAllocator::spawn_delayed_release`
  invoked from `CreateGuard::drop` is structurally broken — the
  task is spawned on the short-lived `detach_isolated`
  ("create-rollbk") private compio runtime that is dropped
  IMMEDIATELY after the cleanup future returns Ready. The detached
  task with a 5s `compio::time::sleep` never gets a chance to fire;
  `Runtime::Drop → Scheduler::clear()` (compio-runtime 0.11.0
  `runtime/mod.rs:389-399` + `runtime/scheduler/mod.rs:233-251`)
  drops pending futures by design. **The vm_index is leaked on
  failed-create until next-controller-boot orphan prune.**
  Production default 5s; the existing unit tests use
  `Duration::ZERO` (mostly synchronous-poll path) and the test
  fixtures keep their outer runtime alive — neither exercises the
  production runtime-drop window. The stop-path call site
  (`stop_inner` line 1324) is **NOT affected** because it runs on
  long-lived runtimes (ntex worker for HTTP stop; `snap-idle-gc`
  isolated runtime that re-sleeps 60s after each iteration).
- **R28-I1 (NEW IMPORTANT)**: T5
  `verify_agent_version_post_restore` inserts a **second 10s
  `spawn_blocking`+ureq slot** between unseal and
  `clock_resync_post_restore` inside the wake state machine's
  `ClockResyncing` phase. The two ureq calls are SERIAL despite
  hitting the SAME agent URL with the SAME signing key — wall time
  on the happy path now stretches by up to 10s on a hung agent (one
  10s budget per probe). The wake_machine runs inside
  `detach_isolated("wake-…")`, so no ntex starvation, but the
  user-visible Restoring phase budget (default 120s
  `alloc_running_timeout_secs` + per-probe budgets) gets eaten
  faster. Operator-facing: the SLO p99 for wake-to-running shifts
  right.
- **R28-I2 (NEW IMPORTANT)**: T5's `Skipped { reason:
  "transport_error" }` swallows the SAME failure shape that the
  pre-T5 path would have surfaced via `livez` (a half-dead agent
  that answers TCP but not HTTP). Concurrency-side concern: a wake
  that lands on a half-dead agent now races the T5 probe's 10s
  timeout against `clock_resync_post_restore`'s timeout — both
  serial against the same dead socket. The wake takes 2× the time
  to fail what it used to. Not a correctness bug — `Skipped` is
  documented as "transport error → proceed" — but the LATENCY
  fingerprint of a stale wake just doubled on the failure path.
- **R28-M1 (NEW MINOR)**: T5 probe's `spawn_blocking` ureq slot is
  marked cancel-unsafe by convention — but wake_machine's
  `detach_isolated` thread has no cancellation surface in
  production, so this is observation-only. The function would be a
  cancel-unsafety hazard if extracted and called from any handler
  with a client-driven deadline.
- **R28-M2 (NEW MINOR)**: `spawn_delayed_release` accepts
  `reason: &'static str` and a sandbox_id, but on the
  `CreateGuard::drop → detach_isolated` short-runtime path the log
  line at line 378-384 emits NOTHING in production (the task is
  dropped before its async body runs). Observability gap that
  amplifies R28-C1.
- **R28-M3 (NEW MINOR)**: `Pool::start_housekeeper` (`8c0b361e`)
  on both pool roles — concurrency-recheck CLEAN. The housekeeper
  holds `Weak<Pool>` and self-terminates on last-strong-Rc-drop;
  the thread-local `Rc<Pool>` cache is per-compio-worker so each
  thread mints its own housekeeper. No cross-thread coordination,
  no Send/Sync violation, no risk of double-spawn (`open_pool` is
  the only call site for the sandbox_app role, gated by the
  thread-local OnceCell pattern). See R28-DEF1 for the
  perf-adjacent note.
- **R28-DEF1 (NEW DEFER)**: idle-conn count under stress-r7-A pg
  500-max-conns + housekeeper. The interaction between the
  thread-local Rc<Pool> cache (N pools per compio-worker count) and
  the housekeeper's reap cadence is a perf-lens question, not a
  concurrency-lens one. Concurrency-r28 raises no objection.
- **R27-I1 confirmation**: `host_dir` mtime baseline shift under
  Phase 2 — STILL OPEN, unchanged (no Phase 2 disposition activity
  in this round's commits). The flag was flipped to true in
  scripts at `231e66c6` PRIOR to the r27 boundary; the production
  code path is governed by `cfg.driver_stages_disk_images`.
- **R27-I2 confirmation**: home.img per-user race under multi-worker
  topology — STILL OPEN, unchanged.

## CRITICAL

### [R28-C1] (NEW CRITICAL) `spawn_delayed_release` inside `CreateGuard::drop` is dropped with the private compio runtime before its 5s sleep can fire

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:363-387` — the
    `spawn_delayed_release` definition: `compio::runtime::spawn` +
    `.detach()` on the AMBIENT current-thread compio runtime.
  - `crates/sandbox/src/backend/nomad_ch.rs:2213-2336` —
    `CreateGuard::drop` dispatches the cleanup tail via
    `crate::detach::detach_isolated("create-rollbk", ...)`. The
    cleanup future ends right after calling
    `spawn_delayed_release` (line 2272-2278). When the cleanup
    future returns Ready, the OS thread's private compio runtime
    is dropped.
  - `crates/sandbox/src/detach.rs:76-110` — `detach_isolated`:
    `let rt = Runtime::new()?; let fut = make_fut(); rt.block_on(fut);`
    The runtime is a local binding — dropped at the end of the
    closure scope.
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
    `Waker`s." The 5s `compio::time::sleep` future is one of
    those dropped tasks.

- **The shape**:

  Production CreateGuard::drop flow:

  ```
  T+0:      try_create() returns Err → CreateGuard's owner drops it
  T+0+ε:    CreateGuard::drop runs (synchronous Rust drop on ntex
              worker thread). Captures vm_index_opt + release_delay (5s
              default) + alloc Arc.
  T+0+ε:    detach_isolated("create-rollbk", make_fut) spawns OS thread.
  T+0+2ε:   OS thread builds compio::runtime::Runtime, calls
              rt.block_on(cleanup_future).
  T+10s:    cleanup_future awaits Nomad purge (up to 10s budget).
              On success, calls VmIndexAllocator::spawn_delayed_release
              which does compio::runtime::spawn(async { sleep(5s);
              release; log; }).detach() on THIS thread's runtime.
  T+10s+ε:  cleanup_future continues, logs "leaking host_dir"
              (host_dir_created branch), returns.
  T+10s+ε:  block_on observes Ready, runs one more scheduler.run()
              cycle (event_interval bounded). The delayed-release
              future polls, registers a 5s timer with compio's timer
              driver, returns Pending.
  T+10s+ε:  block_on returns. `rt` drops at end of closure.
  T+10s+ε:  Runtime::Drop → scheduler.clear() → the pending delayed-
              release future is DROPPED. The 5s sleep never fires.
              The lock().release(i) never runs. The slot stays
              allocated.
  ```

- **Why it's not caught by existing tests**:

  - `spawn_delayed_release_with_zero_delay_releases_immediately`
    (line 4583-4613): zero-delay path. The outer test future
    keeps polling (`compio::time::sleep(5ms)` loop) so the test
    runtime is kept alive while the spawned task runs. Also
    delay==0 means the task body is synchronously ready on first
    poll; this might land within the one extra `self.run()` cycle
    after the outer future is Ready. Either way it doesn't
    exercise the production runtime-drop window.

  - `spawn_delayed_release_honors_configured_delay` (line
    4618-4660): delay=200ms. Outer future awaits `delay/2`
    `compio::time::sleep` then `delay + 500ms` more polling — the
    test runtime stays alive for the full 700ms window. Production
    has NO such outer poll loop; the cleanup future returns and
    block_on exits.

  - `create_guard_releases_vm_index_when_no_job_submitted` (line
    4399-4449): uses `Duration::ZERO`. With `#[compio::test]`
    the test runtime is the outer harness, **not** the
    `detach_isolated` private runtime — the test runs the cleanup
    on the test's main compio runtime via `Drop`. But
    `detach_isolated` does spawn a new OS thread + new Runtime;
    the test polls the OUTER pool waiting for the slot. The
    cleanup task IS detached on the new private runtime; it
    completes (with zero delay) before that runtime drops. **The
    test passes with delay==0 because zero-delay collapses to a
    near-synchronous body that completes within one
    `self.run()` post-Ready cycle.** Bump the test fixture's
    `release_delay` to `Duration::from_millis(50)` and the test
    would expose the bug.

  - `restored_sandbox_is_stoppable_and_releases_vm_index`
    (lines 7414-7449 in the post-r24-A2-S3 update): explicitly
    sets `vm_index_release_delay_secs: 0` in its test config so
    the release fires fast. So even THIS test (which calls
    backend.stop, not CreateGuard) doesn't exercise the
    non-zero-delay path under detach_isolated.

  Summary: production runs with delay=5s through CreateGuard::drop;
  ZERO tests cover that path.

- **Concurrency analysis (other call sites)**:

  1. **`stop_inner` line 1324** (`stop-fence-passed`): the caller
     is `backend.stop(id).await` which is reached from
     `handlers.rs:667` (HTTP stop handler on long-lived ntex
     worker runtime) and `registry.rs:868` (idle GC sweep inside
     `detach_isolated("snap-idle-gc", ...)` BUT the loop body
     re-sleeps `interval=60s` after each `backend.stop(id).await`
     completes — the runtime lives for the next `sleep(60s)` so
     the spawned 5s task runs fine). **OK.**

  2. **`CreateGuard::drop` line 2272** (`create-failure-cleanup`):
     dispatched via `detach_isolated("create-rollbk", ...)` whose
     runtime is SHORT-LIVED — the outer future returns right
     after the spawn call. **BROKEN.**

- **Severity**: CRITICAL. The fix this commit was supposed to
  apply (defense-in-depth against the stress-r8 same-slot
  tap-collision race when retry-CREATE picks up a slot whose
  kernel state isn't fully evicted) is INACTIVE on the very
  failure-path that motivated it. The commit message says
  "Closes the residual stress-r8 race where a CREATE picks up a
  vm_index whose tap netdev / fcntl locks the host kernel hasn't
  finished evicting from the previous tenant" — the CreateGuard
  drop is where that's most likely to bite (a failed-create
  followed by a retry-CREATE from the user is the exact
  same-slot reuse window). And in that path the release just
  silently never happens.

- **Failure shape in production**: vm_index leak. The slot
  stays in `next` allocator state (handed out, not in `freed`).
  Next CREATE for any user gets `next+1`; the leaked slot is
  unreachable until controller restart. Orphan-prune at next
  controller boot is the eventual recovery (per the
  CreateGuard::drop docstring at lines 2295-2308). Less severe
  than a tap collision (which is what r24-A2-S3 was trying to
  prevent on the OTHER side of the race) — but the slot leak
  rate now equals the failed-create rate, which is higher than
  the controller-boot cadence.

- **Practical likelihood**:
  - Steady-state success: zero impact (CreateGuard::drop's
    `armed=false` short-circuits at line 2183-2185 when
    try_create succeeded; the drop never reaches the spawn site).
  - Failed-create on a real failure: 100% leak rate. Every
    failed CREATE leaks its slot. Stress-r7's cycle rate is the
    leak rate.
  - Controller restart: orphan-prune reclaims. Bounded by
    `vm_index_floor..=vm_index_ceil` pool size; if the pool
    saturates before the next restart, fresh CREATEs fail with
    "vm-index allocator exhausted."

- **Recommendations** (no implementation; reviewer-only):

  1. **Block on the delayed release inside the cleanup future**,
     not detach. Sleep + release directly:
     ```rust
     // Inside the CreateGuard::drop cleanup future:
     if purge_ok {
         if let Some(i) = vm_index_opt {
             compio::time::sleep(release_delay).await;
             vm_index_allocator.lock()…release(i);
             tracing::info!(…);
         }
     }
     ```
     This keeps the release inside the `detach_isolated` runtime's
     lifetime. Cost: the cleanup task lives for 5s longer than
     before — but that's already the design intent and the
     dedicated OS thread is the only consumer.

  2. **Alternative**: keep `spawn_delayed_release` for the
     long-runtime call sites (stop_inner) but inline the
     sleep+release for the CreateGuard::drop site. The
     `spawn_delayed_release` API can stay for callers whose
     runtime is known long-lived; CreateGuard::drop documents
     why it must NOT use it.

  3. **Alternative — add an awaitable JoinHandle return**:
     `spawn_delayed_release` returns `Task` instead of detaching;
     CreateGuard::drop awaits it before returning. Same outcome
     as #1 but goes through the spawn-pool path.

- **Cross-lens**:
  - **test-coverage r28**: a test that asserts a non-zero
    `release_delay` actually fires the release under
    `CreateGuard::drop` is the missing oracle. The fixture must
    NOT keep a polling loop alive on a SHARED runtime — it must
    use `detach_isolated` (or equivalent) and observe the
    allocator state AFTER the OS thread joins. ~30 LOC.
  - **arch-r28**: the broader question is whether the r24-A2-S3
    design intent (controller-side delay for defense-in-depth)
    is even reachable from CreateGuard::drop, given the OS-thread
    isolation contract. The detach_isolated thread will exit when
    its work is done; tying defense-in-depth to that thread's
    lifetime extends the thread's lifetime which is fine, but
    architecturally the design intent ("delay release by 5s")
    should be expressed as a wait, not a spawn-and-forget.
  - **observability-r28**: production logs will show "vm_index
    released (r24-A2-S3 delayed)" for stop_inner paths but ZERO
    such log lines for CreateGuard::drop paths. Operator should
    expect failed-create to be CORRELATED with no release log,
    yet the slot is still leaked. If anyone tries to compute
    "released vs leaked" from log lines they will be reading a
    false signal.

- **Carry mechanism**: NEW for r28. CRITICAL.

## IMPORTANT

### [R28-I1] (NEW IMPORTANT) T5 inserts a SERIAL second 10s spawn_blocking-ureq slot inside ClockResyncing phase; not parallelized with clock_resync_post_restore

- **Files**:
  - `crates/sandbox/src/wake_machine.rs:485-548` — the call site.
    `verify_agent_version_post_restore` awaited then (on Match /
    Skipped) `clock_resync_post_restore` awaited. SERIAL.
  - `crates/sandbox/src/restore_handler.rs:3142-3208` — T5
    body: `compio::runtime::spawn_blocking(...).await` with
    `ureq::get(...).timeout(Duration::from_secs(10))`.
  - `crates/sandbox/src/restore_handler.rs::clock_resync_post_restore`
    — pre-existing ureq-via-spawn_blocking call with the same
    shape.

- **The shape**:

  ```
  Phase: ClockResyncing
    unseal sandbox key                       (~ms, pg)
    [T5 NEW] verify_agent_version            up to 10s ureq
    clock_resync_post_restore                up to 10s ureq
    Phase: Registering …
  ```

  Both ureq calls hit `agent_url/{version,clock_resync}`. Same
  HTTP/2 connection could in principle multiplex; ureq is HTTP/1.1
  + new-conn-per-call. On a healthy agent both calls return in
  ~10-50ms each so the serial cost is invisible. On a hung agent
  the wake takes 20s to fail instead of 10s. On a
  partially-degraded agent (e.g. answers /version in 8s, /clock_resync
  in 9s) the wake takes 17s instead of either 9s.

- **Concurrency analysis**:

  1. The wake_machine runs inside `detach_isolated("wake-…")`
     on its own OS thread + private compio runtime. Doubling the
     ureq budget does NOT starve other wakes (they run on their
     own dedicated threads) or HTTP handlers (they run on the
     ntex worker runtime). **No cross-task starvation.**

  2. The user-visible budget is the wake_jobs row's "how long
     until terminal." There's no explicit phase-timeout for the
     ClockResyncing phase in this codebase grep can find;
     operators rely on the polling client's deadline. Doubling
     the worst-case stretches that.

  3. Parallelization is mechanically possible
     (`futures::join!(verify, resync)`) but raises new shapes:
     - If verify=Mismatch, we've already done the clock_resync
       work for nothing (cheap; just bytes-on-wire + small pg
       no-op). Acceptable cost.
     - If resync=Err but verify=Match, the rollback uses
       `WakeErrorCode::ClockResyncFailed`. Same as today. No
       contract change.
     - If verify=Skipped + resync=Err: rollback path identical to
       today. No contract change.
     - If verify=Mismatch + resync=Err: the WakeErrorCode picked
       is whichever the code prefers (verify wins per "rollout
       skew comes first"). One-line code policy.
     - Net: parallelization is concurrency-clean.

- **Severity**: IMPORTANT. The serial cost is small on the happy
  path; the failure-path latency-doubling matters when stress-r7
  hits an agent on a node mid-deploy (the exact partial-rollout
  scenario T5 was designed to catch). The probe correctly catches
  the build-skew — but does so by paying the FULL timeout on the
  dead agent THEN another full timeout on clock_resync.

- **Recommendation** (no implementation):

  1. **Parallelize**: `let (v, r) = futures::join!(verify, resync);`
     and branch on the joint result. Halves the worst-case wall
     time. Concurrency-clean per analysis above.

  2. **Alternative** (simpler): leave serial but document that the
     two probes share an SLO budget — and consider tightening the
     T5 probe's per-call timeout to 5s. Build-skew detection
     does not need 10s of grace; clock_resync's 10s is the
     load-bearing budget.

  3. **Alternative** (defer): no change; the failure-path
     latency-doubling is acceptable for v1. Document in the
     wake_machine ClockResyncing-phase doc-comment.

- **Cross-lens**: arch-r28 owns the parallelize-or-not call.

- **Carry mechanism**: NEW for r28.

### [R28-I2] (NEW IMPORTANT) T5 `Skipped { transport_error }` masks the same half-dead-agent failure shape that livez was supposed to catch

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:3220-3232` — the
    `Skipped { reason: "transport_error" }` branch.
  - `crates/sandbox/src/wake_machine.rs:446-464` — the prior
    livez probe (wait_for_livez under spawn_blocking) which on
    failure returns `WakeErrorCode::LivezTimeout`.

- **The shape**:

  livez succeeds (agent answers something at /livez within budget).
  T5 probe issues GET /version. The agent's TCP socket is alive but
  the HTTP handler is wedged → ureq's 10s timeout trips →
  `Err(transport_error)` → outer match returns
  `Skipped { reason: "transport_error" }` with a WARN log → wake
  PROCEEDS to clock_resync.

  clock_resync then issues another HTTP call against the same
  half-dead agent. Burns its own 10s, fails, rollback with
  `ClockResyncFailed`.

  **Wake takes 20s to fail instead of 10s.** And the failure
  attribution is `ClockResyncFailed` not `AgentVersionMismatch` —
  which is correct (it's not a SHA mismatch, it's a hang) — but
  also not maximally informative.

- **Concurrency angle**:

  No new race, no new shared state. The concern is:
  - Two SERIAL HTTP probes against the same dead socket → 2x
    wall time on the failure path (overlaps R28-I1).
  - The `Skipped` outcome obscures the early signal that the
    agent is unresponsive. If T5's probe had returned
    `WakeErrorCode::AgentUnresponsive` (a distinct code), the
    operator would have a 10s-faster path to "this node is dead;
    drain it" vs. waiting another 10s for clock_resync to fail.
  - Half-dead-agent is the EXACT scenario the wake state machine
    is meant to attribute precisely — between `livez_timeout`,
    `restore_backend_failed`, `clock_resync_failed`, and now
    `agent_version_mismatch`, the SLO dashboard has 4 buckets.
    A 5th "agent_unresponsive_post_livez" bucket may be
    valuable; flagging.

- **Severity**: IMPORTANT (concurrency-adjacent —
  latency-on-failure shape; debatably observability-lens).

- **Recommendation**:

  1. Document the latency-on-failure compound: a half-dead agent
     now takes 20s+ to fail vs 10s pre-T5. Operator runbook
     should reflect.

  2. Consider a `Skipped` sub-classification at the
     wake_machine level: if `Skipped { reason: "transport_error" }`
     AND clock_resync subsequently fails → emit a metric
     `sandbox_wake_agent_half_dead_total` so the operator can
     route this distinct fingerprint.

  3. Alternative: keep the current behaviour but add a
     wake-machine-level fast-path: if T5 returns
     `Skipped { transport_error }`, SKIP clock_resync too and
     attribute to a new `AgentUnresponsivePostLivez` code. The
     half-dead agent is going to fail clock_resync anyway; we
     might as well save 10s.

- **Cross-lens**:
  - **observability-r28**: bucket proposal owned here.
  - **arch-r28**: the design call on whether
    `transport_error → Skipped` is the right disposition is the
    arch-lens question. Concurrency-r28 raises the
    latency-compounding angle.

- **Carry mechanism**: NEW for r28.

## MINOR

### [R28-M1] (NEW MINOR) `verify_agent_version_post_restore` is cancel-unsafe by convention but reaches no cancellation point in production

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:3142-3208` — the
    `spawn_blocking` ureq slot.
  - `crates/sandbox/src/wake_machine.rs:499-505` — the call site,
    inside `WakeMachine::drive` which is dispatched via
    `detach_isolated`.

- **The shape**:

  `compio::runtime::spawn_blocking` is documented as
  "task will not be cancelled even if the future is dropped"
  (compio-runtime 0.11.0 `runtime/mod.rs:208`). So if the
  await-side future is dropped, the ureq call continues in the
  background and the result is discarded. The signing key bytes
  are MOVED into the closure (`SigningKey::from_bytes`); no
  shared state escapes. **No correctness defect** — the dropped
  result is dropped cleanly.

  But: cancellation reaches the wake_machine drive only if the
  `detach_isolated` thread's compio runtime is dropped, which
  happens only when `drive()` returns. By definition the call
  site is INSIDE `drive()`; there's no external cancellation
  signal. So in production this is a non-event.

- **Why MINOR not IMPORTANT**: same pattern as
  `clock_resync_post_restore` which has the same shape and has
  been audited as concurrency-clean for many rounds. T5's probe
  is structurally identical.

- **Severity**: MINOR (hygiene; flagged for future-proofing if
  any caller extracts the probe to a handler with a client
  deadline).

- **Carry mechanism**: NEW for r28.

### [R28-M2] (NEW MINOR) `spawn_delayed_release` log line lost on the CreateGuard::drop path (sibling of R28-C1)

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:378-384` — the
    `tracing::info!(... "vm_index released (r24-A2-S3 delayed)")`
    log line inside the spawned future.
  - `crates/sandbox/src/backend/nomad_ch.rs:2272-2278` — the
    CreateGuard::drop call site.

- **The shape**: the log line lives INSIDE the spawned future.
  Per R28-C1, that future is dropped before it can fire on the
  CreateGuard::drop path. Operators scanning the log stream for
  "released" events after a failed CREATE will see NOTHING —
  even though the new code would have them believe the release
  was just delayed by 5s (with subsequent log line).

- **Why MINOR**: observability follow-on of the R28-C1
  correctness defect. Fixing R28-C1 (inline sleep+release inside
  cleanup future) fixes this automatically.

- **Severity**: MINOR.
- **Carry mechanism**: NEW for r28; closes with R28-C1.

### [R28-M3] (NEW MINOR) `Pool::start_housekeeper` on both pool roles — concurrency-lens recheck CLEAN

- **Files**:
  - `crates/sandbox/src/db.rs:629-639` (`open_pool`) and
    `db.rs:667-670` (`pool_audit`) — `pool.start_housekeeper()`
    called immediately after `Pool::connect_with_config` and
    BEFORE `install_pool`.
  - `target/.../compio-postgres-0.x/src/pool.rs:341-353` —
    `start_housekeeper` takes `&Rc<Self>`, spawns a detached
    `compio::runtime::spawn` task that holds `Weak<Self>` and
    self-terminates when the last strong Rc drops.

- **Analysis**:

  1. `Pool::start_housekeeper(&Rc<Self>)` requires the caller
     to hold an `Rc<Pool>`, which the cache already does. The
     thread-local cache (`POOL_APP_CELL`, `POOL_AUDIT_CELL`) is
     the strong-Rc-holder for the rest of the thread's lifetime.

  2. The race-loser pool: when two tasks on the same compio
     worker concurrently miss the cache, both build their own
     `Pool`. The race-loser's `Rc<Pool>` drops at the end of its
     call (the `Result<Rc<Pool>>` is overwritten by
     `install_pool`'s tiebreaker). The housekeeper held a
     `Weak`, so its first attempted upgrade fails and the
     housekeeper task exits. **Clean wind-down.**

  3. Two housekeepers per thread (one for sandbox_app, one for
     sandbox_audit) is the steady state. They're independent;
     no cross-coordination.

  4. The detached `compio::runtime::spawn(...).detach()` inside
     `start_housekeeper` runs on the CURRENT compio runtime —
     which is the ntex worker's long-lived runtime for HTTP
     paths and the per-thread isolated runtime for
     `detach_isolated`-spawned tasks. For ntex workers: fine,
     long-lived. For `detach_isolated`: the thread-local
     `POOL_APP_CELL` is per-thread, so if a `detach_isolated`
     thread calls `open_pool` for the first time, the
     housekeeper spawns on that short-lived runtime.

     **Sub-shape**: same lifecycle issue as R28-C1, but inverted
     — the housekeeper holds `Weak<Pool>`, so when the thread
     exits and the strong-Rc drops, the housekeeper's next
     upgrade fails and it exits. No leak. **But** the
     housekeeper may NEVER get to RUN even one
     idle-conn-reap cycle if the thread is very short-lived.
     Mitigation: the thread holds its connections only as long
     as it lives; on thread exit the connections close via
     `Drop`. Idle-conn reaping is a NO-OP for short-lived
     threads.

- **Severity**: MINOR (concurrency-clean observation; perf-lens
  may want to revisit the short-lived-thread cost).

- **Cross-lens**: perf-r28 owns the cost-on-short-lived-thread
  question (R28-DEF1).

- **Carry mechanism**: NEW for r28.

## DEFER

### [R28-DEF1] (NEW DEFER) Idle-conn count under stress-r7-A 500-max-conns + per-thread Rc<Pool> + housekeeper interaction

- **Files**:
  - `crates/sandbox/scripts/...` (out-of-scope) — pg
    `max_connections=500`, `shared_buffers=1GB`.
  - `crates/sandbox/src/db.rs:629,667` — both
    `start_housekeeper` call sites.

- **The question**: with N compio workers × 2 pools per worker ×
  pool_max conns per pool, how many idle conns linger? The
  housekeeper reaps idle conns past `idle_timeout` (default 600s
  per compio-postgres `PoolConfig`). At c=20 stress with 8
  ntex workers + 8 isolated detach threads, worst case is ~32
  pool instances. If pool_max=8 per pool and idle_timeout=600s,
  the steady-state idle conn count under bursty traffic could
  approach 256 conns — well under 500 but the headroom is
  thinner than the bump suggests.

- **Severity**: DEFER (perf-lens). Concurrency-r28 has no
  finding here.

- **Carry mechanism**: NEW for r28 cross-lens defer.

## Round-briefing answers (summary)

### 1. r24-A2-S3 VmIndexAllocator release DELAY: cancellation semantics? 5s correctness given driver's 5s tap-verify budget?

**Cancellation: BROKEN on CreateGuard::drop path** (R28-C1). The
spawned task is cancelled at the runtime drop boundary; the 5s
sleep never fires. The stop-path call sites (stop_inner via HTTP
handler / snap-idle-gc loop) are fine because their runtimes are
long-lived.

**5s correctness vs driver's 5s tap-verify budget**: structurally
sound IF the release actually happens. The driver's r24-A2-S2
verify gate closes the worker-side tuntap-add window; the
controller-side 5s adds defense-in-depth for host_dir GC, sweeper
races, etc. The two ~5s budgets are deliberately matched so the
controller doesn't release until the driver has finished its
synchronous netdev cleanup. But: the controller-side delay is
ABSENT on the CreateGuard::drop path (R28-C1) — so failed-create
followed by retry-CREATE for the same user has only the driver-
side gate, not the controller-side defense-in-depth.

Window overlap analysis: the CRITICAL race the r24-A2-S3 commit
was supposed to close (failed-create A → retry-CREATE B picks up
A's slot → tap collision because kernel hasn't evicted) is NOT
closed by this commit on the precise failure-path that matters.

### 2. T5 verify_agent_version_post_restore between unseal and clock_resync: cancel-unsafe?

**Cancel-unsafe by spawn_blocking convention** (R28-M1), but no
cancellation point in production (wake_machine runs in
`detach_isolated`). Same shape as `clock_resync_post_restore`
which has been audited clean. Hygiene-flag only.

### 3. R27-P1 T5 parallelization: any race?

**No new race introduced by parallelizing** (R28-I1 analysis).
Both ureq calls hit the same agent URL with the same signing
key; mutex contention zero; outcome combination is policy-
deterministic. `futures::join!(verify, resync)` is
concurrency-clean. The serial cost on the failure path is
non-trivial — R28-I1 recommends parallelizing OR tightening
T5's 10s timeout to 5s.

### 4. r28-A1 host_dir lifecycle split-brain: documentation or race?

**Documentation drift, not race**. The 3 ambiguities flagged by
arch r28 (home.img wire field, sweeper rustdoc, host_dir_created
bool overload) are observability/clarity issues — R27-M1 already
captured the host_dir_created semantic-overload; the home.img
wire field and sweeper rustdoc are arch-lens questions. The
sweeper's behaviour is governed by row terminal-state + grace +
DB-row predicates (independent of who created the dir);
concurrency-r28 sees no actual race hiding in the
documentation drift.

## Items NOT findings (verified clean this round)

### [N/A] T5 wake state machine integration

`wake_machine.rs:485-548`. The phase ordering (livez → unseal →
T5 → clock_resync → register) is structurally sound. The state
machine still serializes through `Phase::Failed`/`Phase::Ok`;
the new `WakeErrorCode::AgentVersionMismatch` routes through the
existing rollback path (`rollback_with`). No new cross-task
state surface introduced.

### [N/A] `WakeErrorCode::AgentVersionMismatch` migration 0014

`db.rs:1655-1671` + `0014_wake_jobs_agent_version_mismatch_code.sql`.
The pg CHECK constraint is widened, not narrowed. Backward-
compatible with any in-flight wake_jobs row at migration
time. Concurrency-clean.

### [N/A] r24-A2-S3 stop-path call site (stop_inner)

`backend/nomad_ch.rs:1324`. Reached from
`handlers.rs:667` (HTTP stop, ntex worker runtime — long-lived)
and `registry.rs:868` (sweep loop inside
`detach_isolated("snap-idle-gc")` whose outer `loop { sleep(60s)
... }` keeps the runtime alive). The 5s delayed-release task
DOES fire in both call sites. Verified clean.

### [N/A] `Pool::start_housekeeper` race-loser cleanup

`db.rs:629,667`. Race-loser pool's `Rc` drops at function exit;
housekeeper's `Weak` fails first upgrade; task exits. No leak.
Verified clean.

### [N/A] R26-C1 thread-local Rc<Pool> pg-gated tests (`871752c7`)

Pure tests; no production-code change; concurrency-clean.

## Cross-lens consensus

- **arch-r28 owns**: (1) R28-I1 parallelize-or-tighten-timeout
  decision; (2) R28-I2 `Skipped { transport_error }`
  disposition + half-dead-agent error-code proposal; (3) the
  R27-I1/I2/M1 carry items unchanged.
- **test-coverage r28 owns**: (1) R28-C1 missing oracle —
  ~30 LOC test asserting `CreateGuard::drop` with non-zero
  delay actually releases the vm_index post-OS-thread-join;
  (2) R25-I1 carry from r27.
- **perf-r28 owns**: R28-DEF1 idle-conn-count interaction;
  R26-C1 thread-local-pool teardown cost on short-lived
  detached tasks (carry from r27).
- **observability-r28 owns**: R28-M2 missing log line on
  CreateGuard::drop (closes with R28-C1); R28-I2 metric proposal
  for half-dead-agent post-livez.
- **code-quality r28 owns**: R27-M1 host_dir_created doc-comment
  carry; R24-I1 fsync_dir doc-misframe carry.
- **stress-r7 cluster**: with the flag flipped to true in scripts
  at `231e66c6`, the cluster behaviour shifts to driver-staged.
  Stress-r7 will exercise the post-Phase-2 race surface. **A
  specific request**: if stress-r7 surfaces a wedge with
  "vm_index exhausted" symptom after sustained failed-create
  pressure, R28-C1 is the suspect. The leak rate equals the
  failed-create rate; pool saturation lands within
  `(ceil - floor + 1) / failed_create_rate` time.

## Carry table

| Finding | Source | r28 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry |
| R20-C1 (Failed side) | r20 → CLOSED r22+r23 e2e | fully CLOSED |
| R20-C1 / R25-I2 (Ok side) | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| R20-I2 sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 10th cycle** |
| R20-I3 / R21-I1 / R24-M2 Restoring watchdog | r20→r26 | **STILL OPEN — 10th cycle** (promote IF stress-r7 cold-cache stretches >60s) |
| R22-I1 observability-gap | r22 → CLOSED r23 | CLOSED carry |
| R22-M3 cross-controller claim | r22 minor | carry |
| R23-I1 (Failed side) | r23 → CLOSED r24 | CLOSED carry |
| R24-I1 fsync_dir doc-misframe | r24 NEW-IMP | **STILL OPEN — code-quality r28 owns** |
| R24-M1 sweep × machine race-into-terminal | r24 minor | carry |
| R24-M3 stranded taps (C-N-W2) | r24 minor | carry |
| R25-I1 sweeper TOCTOU invariant untested | r25 NEW-IMP | **STILL OPEN** (Phase 2 does NOT obsolete; carry to r29) |
| R25-I2 Phase::Ok terminal-overwrite divergence | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R25-M1 typed StagingPathMissing carry-through | r25 minor | carry (doc-comment ask) |
| R26-I1 boot-lookup serial-await + external-dep | r26 NEW-IMP | **STILL OPEN — PRIORITY HOLDS, reassess at Phase 5** |
| R26-M1 from_config_full threading consistent | r26 minor | CLOSED-with-watchful-eye |
| R26-M2 R25-T4 helper extraction race-clean | r26 minor | CLOSED-with-watchful-eye |
| R26-M3 read_snapshot_row DRY (concurrency angle) | r26 minor | carry (code-quality owns) |
| R26-DEF1 perf-r25 C1 concurrency recheck | r26 defer | CLOSED-with-watchful-eye (now Rc<Pool>; R27-DEF1) |
| R27-I1 host_dir mtime baseline shifts | r27 NEW-IMP | **STILL OPEN — arch-r28 owns** |
| R27-I2 home.img per-user race + Phase 5 latent | r27 NEW-IMP | **STILL OPEN — arch-r28 owns** |
| R27-M1 host_dir_created semantic-overload | r27 minor | carry |
| R27-M2 wait_for_alloc_running budget shift | r27 minor | carry (operator-doc hygiene) |
| R27-M3 R25-I1 sweeper TOCTOU shape SHIFTS | r27 minor | confirmation (no change) |
| R27-DEF1 R26-C1 thread-local Rc<Pool> | r27 defer | CLOSED-with-watchful-eye (housekeeper recheck R28-M3) |
| **R28-C1** spawn_delayed_release lost on CreateGuard::drop | r28 NEW-CRIT | NEW |
| **R28-I1** T5 + clock_resync SERIAL; not parallelized | r28 NEW-IMP | NEW |
| **R28-I2** T5 Skipped{transport_error} → 2x failure latency | r28 NEW-IMP | NEW |
| R28-M1 verify_agent_version cancel-unsafe by convention | r28 minor | NEW (hygiene) |
| R28-M2 missing log line on CreateGuard::drop release | r28 minor | NEW (closes with R28-C1) |
| R28-M3 start_housekeeper concurrency-recheck | r28 minor | NEW (CLEAN) |
| R28-DEF1 idle-conn count under 500-max-conns | r28 defer | NEW (perf-r28 owns) |
| Older minors (R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1, R23-M1, R24-M5, R25-M2, R25-M3) | older minor | carry |

## Status block

```
Round 28 (r24-A2-S3 VmIndexAllocator delay LANDED +
          T5 verify_agent_version_post_restore LANDED +
          start_housekeeper followup LANDED +
          stress-r7 IN FLIGHT, flag flipped to true):

  CLOSED:
    R27-DEF1 R26-C1 thread-local Rc<Pool> — concurrency-clean
      confirmed (housekeeper wiring added; race-loser pool
      drops Weak cleanly).

  NEW CRITICAL:
    R28-C1 spawn_delayed_release inside CreateGuard::drop
      is dropped with the private compio runtime before its
      5s sleep can fire — vm_index leak on failed-create.

  NEW IMPORTANT:
    R28-I1 T5 serial 10s probe + clock_resync 10s = doubled
      failure-path latency; parallelizable
      (futures::join!) is concurrency-clean.
    R28-I2 T5 Skipped{transport_error} masks half-dead-agent
      fingerprint; serial second probe doubles failure
      latency. Half-dead-agent should get its own bucket.

  NEW MINOR:
    R28-M1 verify_agent_version_post_restore cancel-unsafe by
      spawn_blocking convention; no production cancel point.
    R28-M2 release log line lost on CreateGuard::drop
      (closes with R28-C1).
    R28-M3 Pool::start_housekeeper recheck CLEAN
      (Weak<Pool> + per-thread Rc semantics).

  NEW DEFER:
    R28-DEF1 idle-conn count interaction with stress-r7-A
      500-max-conns (perf-lens).

  STILL OPEN (carry):
    R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1,
    R27-I1, R27-I2, R27-M1, R27-M2.

  ASK:
    (1) test-coverage r28: oracle for R28-C1 — ~30 LOC test
        asserting CreateGuard::drop with non-zero delay
        actually releases vm_index post-OS-thread-join.
        DO NOT just bump delay to 50ms in the existing test;
        the existing test's outer poll loop keeps the test
        runtime alive past the delay, which is NOT the
        production lifecycle.
    (2) arch r28: R28-C1 design call — inline sleep+release
        inside the CreateGuard cleanup future OR
        spawn-and-join via Task return. The "spawn and
        forget" shape is incompatible with detach_isolated.
    (3) arch r28: R28-I1 parallelize-or-tighten-timeout
        decision.
    (4) arch r28: R28-I2 half-dead-agent bucket proposal.
    (5) arch r28 (carry from r27): Phase 5 r3-A Constraints
        disposition (R27-I2 still argues KEEP).
    (6) arch r28 (carry): R27-I1 doc-comment vs rename for
        host_dir_created.
    (7) arch r28 (carry): R26-I1 boot-lookup shape carry.
    (8) arch r28 (carry): R25-I2 STILL OPEN-PRIORITY-FALLS.
    (9) code-quality r28: R24-I1 + R25-M1 + R26-M3 + R27-M1.
    (10) observability r28: R28-I2 + R28-M2 + R27-I1 metric
         proposals.
    (11) perf r28: R28-DEF1 idle-conn interaction.
    (12) cluster T-8b-stress-r7: if stress-r7 surfaces
         "vm_index exhausted" symptom under sustained
         failed-create pressure, R28-C1 is the suspect.
         Pool saturation at (ceil-floor+1)/failed_create_rate.
```

## ASK clarifications for the user

Four open design questions concurrency-r28 raised that need owner
calls before r29:

1. **R28-C1 design**: inline the sleep+release inside the
   CreateGuard cleanup future (simplest, breaks the
   spawn-and-forget API uniformity but tied to detach_isolated's
   thread lifetime correctly) OR change `spawn_delayed_release`
   to return a `Task` and have CreateGuard::drop await it (keeps
   API uniformity but extends `detach_isolated` thread lifetime
   by the configured delay — acceptable since the thread is
   already dedicated). Concurrency-r28 mildly prefers option A
   (inline) because option B encodes the "must await" contract
   in caller code, which is easy to miss in future call sites.

2. **R28-I1 disposition**: parallelize via `futures::join!`
   (halves worst-case failure latency, concurrency-clean) OR
   tighten the T5 probe's 10s timeout to 5s (cheaper change,
   still leaves the latency-doubling but smaller). Mixed signal
   from arch / observability lenses on whether
   `agent_version_mismatch` deserves its own latency budget.

3. **R28-I2 disposition**: keep `Skipped { transport_error }`
   semantics OR fast-fail with a new `AgentUnresponsivePostLivez`
   code on the half-dead-agent fingerprint. The latter saves
   ~10s on the failure path and surfaces a distinct operator
   bucket.

4. **Carry items**: R27-I1 (host_dir mtime + observability),
   R27-I2 (home.img per-user under Phase 5), R26-I1 (boot
   lookup) — all unchanged this round. arch-r28 owns; user
   confirm whether to fold these into a Phase 5 disposition
   doc or leave as carry through r29.
