# Sandbox/snapshot-restore — concurrency r30 review

Date: 2026-05-25 (UTC).
HEAD at audit: `0ee106d2` (worktree `.worktrees/sandbox-snapshot-restore`,
branch `feat/sandbox-snapshot-restore`).
Round 30 of N. READ-ONLY.

Scope since r29 (`b8310356` → `0ee106d2`, sandbox-crate only):

- **R29-C1 + r29-A2 class-fix** (`62b083e1`) — replaced
  `VmIndexAllocator::spawn_delayed_release` with two helpers:
  `release_vm_index_after(...).await` (inline) and
  `spawn_delayed_release_in_worker(...) -> Task` (typed
  JoinHandle). `stop_inner:1393` and `CreateGuard::drop:2358` both
  route through `release_vm_index_after(...).await`. The original
  fire-and-forget helper is deleted; no production caller uses the
  Task variant (kept as `#[allow(dead_code)]` typed escape hatch).
- **R28-API2 sweep** (`9ac5b850`) — cfg-gated 5 test-scaffolding
  `pub` items under `test-support` feature. No concurrency
  surface change; out of scope here.
- **R29-P1 fix** (`81b6e689`) — parallelised `snap-idle-gc`'s
  per-tick `backend.stop(id).await` loop using
  `futures::future::join_all` at cap=8 (`GC_STOP_CONCURRENCY`).
  Extracted `trait GcStopper` + `async fn gc_stop_chunked(ids,
  stopper, cap)`; production wires `AppStateGcStopper` (Arc to
  AppState; runs the verbatim per-id sequence from the pre-R29-P1
  serial loop).
- **deferred-backlog paperwork** (`e38d553d`) and pin bump
  (`0ee106d2`) — docs / scripts. Out of scope.

In flight: stress-r9-retry-6 (post-R29-P1); c=4 cluster smoke shows
WAKE rootfs.img lock-held wedge (DRIVER-side, kernel `__fput`).

Prior: `…concurrency-2026-05-25-r29.md`.

## Summary

- **5 findings** this round (**0 NEW CRITICAL**, 1 NEW IMPORTANT,
  3 NEW MINOR, 1 verification-clean).
- R29-C1 fix verified concurrency-clean (see `[N/A]` below).
- R29-P1 parallel snap-idle-gc verified concurrency-clean against
  the three brief-asked properties (panic-safety per-task,
  shared-mutable-state races, GcStopper trait cancel-safety) —
  with **one structural observation (R30-I1)** about the
  panic-blast-radius amplification.
- WAKE rootfs.img wedge: brief asked whether the CONTROLLER's WAKE
  state machine could exacerbate the kernel `__fput` race —
  **answer: no NEW race is introduced by R29-P1/R29-C1; the
  existing 5 s `vm_index_release_delay_secs` barrier is the only
  controller-side kernel-settling pause and R29-C1 actually makes
  it more reliable (now bound by `.await`, not detached)**. See
  R30-M1 for the observability shape.
- freed-set race with concurrent `release_vm_index_after`
  callers: **none** — the `Mutex<VmIndexAllocator>` linearises
  every `release()` call, and the sleep happens outside the
  critical section.

## CRITICAL

(None this round.)

## IMPORTANT

### [R30-I1] (NEW IMPORTANT, observational) `gc_stop_chunked`'s `join_all` amplifies panic blast radius from 1 sandbox to up to `cap=8` mid-teardown sandboxes per panicking iteration

- **Files**:
  - `crates/sandbox/src/registry.rs:1001-1006` — the
    `futures::future::join_all` driver:
    ```rust
    async fn gc_stop_chunked(ids: &[Uuid], stopper: &dyn GcStopper, cap: usize) {
        let cap = cap.max(1);
        for chunk in ids.chunks(cap) {
            let _ = futures::future::join_all(chunk.iter().map(|id| stopper.stop_one(*id))).await;
        }
    }
    ```
  - `crates/sandbox/src/registry.rs:961-987` —
    `AppStateGcStopper::stop_one`. The `state.backend.stop(id).await`
    call at line 975 is NOT wrapped in `catch_unwind` (only the
    final `state.sandboxes.remove(&id)` is, at 982-984). A panic
    inside `stop` (e.g. an `unwrap_or_else(p| p.into_inner())`
    inside `stop_inner` that re-panics on a doubly-poisoned lock;
    a `.unwrap()` on a malformed sealed record during `unseal`
    inside the `persist.delete` tail) propagates up out of
    `stop_one`'s future.
  - `crates/sandbox/src/registry.rs:1016-1048` — the main
    `snap-idle-gc` loop hosting the `gc_stop_chunked` call. The
    loop has NO `catch_unwind` around `gc_stop_chunked`. A panic
    propagates to `detach_isolated`'s `rt.block_on(fut)` (per
    `detach.rs:100`); compio's `block_on` does not catch panics
    from the polled future. The OS thread unwinds; per
    `detach.rs:170-187`'s test fixture, the panic is contained
    to the thread — the controller stays alive but the GC loop
    is dead until next-controller-boot.

- **Concurrency analysis**:

  Pre-R29-P1 the loop was strictly serial. A panic in
  `state.backend.stop(id_A).await` killed the GC loop and left
  `id_A` mid-teardown (Nomad job possibly partially purged,
  vm_index possibly held). **Blast radius: 1 sandbox.**

  Post-R29-P1, `join_all(chunk.iter().map(|id| stopper.stop_one(*id)))`
  holds up to `cap = 8` futures concurrently. If `stop_one(id_A)`
  panics while `stop_one(id_B..H)` are mid-`stop_inner` await
  (e.g. inside `wait_for_job_gone` polling Nomad, or inside
  `wait_for_agent_silent` polling host_fence, or inside the 5 s
  `compio::time::sleep` of `release_vm_index_after`), the panic
  propagates up through `join_all`. `join_all`'s drop runs first
  — and **dropping a pending future cancels its in-flight awaits
  silently**: the in-flight HTTP request to Nomad is abandoned
  (request bytes already sent, response never read), the
  vm_index `release()` call never fires for sandboxes B..H. The
  per-loop-iteration loss is `min(cap, |chunk|) = 8` sandboxes
  mid-teardown, not 1.

  The orphan-prune at next controller boot eventually reclaims
  the leaked vm_index slots — but in the stress-r9-style
  workloads that motivated R29-P1, "next controller boot" is the
  failure mode R29-P1 closed. Amplifying a panic's blast radius
  from 1 → 8 vm_index leaks across a Nomad-restart-cycle is a
  structural regression in panic-recovery semantics, even if no
  panic path is currently reachable in production.

- **Severity**: IMPORTANT (structural — the blast radius
  amplification is real, the trigger is unlikely-but-not-zero).
  Today no `.unwrap()` / `.expect()` / panic-fast path on the
  `state.backend.stop(id).await` hot path is known reachable in
  prod. But: `stop_inner` reaches `persist.delete` (which calls
  through to `compio::fs::remove_file` — file-not-found returns
  Err, not panic — ok), `release_vm_index_after`'s lock
  (`unwrap_or_else(|p| p.into_inner())` — handles poison
  gracefully — ok), `wait_for_job_gone`'s ureq calls (typed
  errors, no panic — ok). Conclusion: **trigger is latent, not
  imminent.** The structural amplification is what makes this
  IMPORTANT.

- **Recommendations** (no implementation; reviewer-only):

  1. **Wrap each `stop_one` future in `catch_unwind` inside the
     `AppStateGcStopper::stop_one` body** (around the
     `backend.stop(id).await` and the `sandboxes.remove(&id)`
     tail). This contains the panic at the per-id boundary so
     `join_all` sees a normal `Ok(())` result and the surviving
     7 in-flight stops finish their teardowns. The same pattern
     the sweeper's `snapshot_one` callers use today — but the
     standard `catch_unwind` doesn't compose cleanly with
     async-await on a `!Send` `dyn Future` (the future itself
     must implement `UnwindSafe`, which boxed-async futures do
     not). Use `futures::future::FutureExt::catch_unwind` which
     wraps the future in `AssertUnwindSafe` internally and
     returns `Result<T, Box<dyn Any + Send>>`. `futures` is
     already a workspace dep.

  2. **Alternative**: wrap each per-id call via
     `compio::runtime::spawn(stopper.stop_one(*id))` (which
     panic-catches automatically — see
     `spawn_delayed_release_in_worker`'s rustdoc at
     `nomad_ch.rs:434-443` confirming the
     `Result<T, Box<dyn Any + Send>>` wrapper from
     `compio::runtime::spawn`). But spawn requires the future
     to be `'static` (no `&self` borrow) — would require a
     larger refactor than (1).

  3. **Test oracle**: a `PanickingGcStopper` variant for the
     existing
     `gc_stop_chunked_is_actually_concurrent` test that has
     `stop_one(id_0)` panic and `stop_one(id_1..7)` sleep. Assert
     post-completion that all 7 sibling sleeps completed (or
     equivalently, all 7 `in_flight.fetch_sub(1)` calls landed).
     Without (1) or (2), this oracle would catch the
     amplification by observing that `in_flight` is
     non-decreasing after the panic. ~25 LOC.

- **Cross-lens**:
  - **test-coverage r30 owns**: the panic-oracle test from
    recommendation (3) above. The current
    `gc_stop_chunked_is_actually_concurrent` test pins the
    happy-path concurrency level but not the panic-recovery
    contract.
  - **arch-r30**: the per-id panic isolation contract should
    arguably be expressed in the `GcStopper` trait itself (the
    trait commits each impl to panic-safe per-id execution; the
    test substrate verifies). Arch-r30 owns the trait-contract
    call.

- **Carry mechanism**: NEW for r30; LIKELY CLOSES with the next
  registry-side hardening pass.

## MINOR

### [R30-M1] (NEW MINOR — WAKE rootfs.img wedge, controller-side analysis) controller does NOT add any kernel-`__fput`-settling barrier beyond `vm_index_release_delay_secs`; R29-C1's `.await` change is strictly defensive

- **Files**:
  - `crates/sandbox/src/backend/nomad_ch.rs:1393-1402` — the
    post-R29-C1 `release_vm_index_after.await` site in
    `stop_inner`. The 5 s `vm_index_release_delay_secs`
    (production default) is the only post-fence-cleared,
    pre-release pause.
  - `crates/sandbox/src/backend/nomad_ch.rs:1370-1379` — the
    pre-r29 rationale comment for the delay:
    > host kernel has time to evict the tap netdev / drain
    > fcntl locks from this tenant's CH process before a fresh
    > CREATE picks up the same vm_index.

  - `crates/sandbox/src/restore_handler.rs:527-585` —
    `reserve_vm_index_with_retry` polls the allocator until
    teardown's `release()` fires. Once it succeeds,
    `do_restore_inner` immediately moves to
    `backend.restore_alloc_dir(sandbox_id)` and
    `submit_restore_job` — NO additional kernel-settling
    barrier.
  - `crates/sandbox/src/admin_handlers.rs:1486-1506` — admin
    snapshot returns 200 to the client BEFORE teardown
    completes (detach_isolated background). Client can
    immediately POST `/wake`; wake's `reserve_vm_index_with_retry`
    retries until the teardown's `release()` fires.

- **The shape (brief-asked angle)**:

  Brief: "c=4 cluster smoke reveals WAKE rootfs.img lock-held
  wedge … DRIVER-side (kernel `__fput` on rootfs.img).
  Concurrency lens: is the CONTROLLER's WAKE state machine
  doing anything that could exacerbate this? E.g., the wake
  submit fires immediately after snapshot completes; if
  Nomad's transition is faster than kernel cleanup, the wake
  races."

  **Controller-side answer**: the wake's `reserve_vm_index_with_retry`
  cannot succeed until the teardown's
  `release_vm_index_after.await` has slept the full
  `vm_index_release_delay_secs` (5 s prod default) PAST the
  host_fence-cleared signal. **R29-C1 made this barrier MORE
  reliable** — pre-R29-C1, the timer task was detached and (on
  the snap-teardown short-lived runtime) could be dropped before
  the 5 s elapsed, in which case the vm_index leaked and the
  wake's reserve never succeeded against the leaked slot until
  next-boot orphan-prune. The R29-C1 `.await` shape makes the
  5 s pause guaranteed to elapse before release.

  **However**: the 5 s pause is the ONLY post-fence-cleared,
  pre-release barrier. If the kernel needs MORE than 5 s of
  post-`/livez`-silent settling to finish `__fput` on the
  rootfs.img fd (under c=4 stress this is the observed wedge
  shape), the wake's `submit_restore_job` still races kernel
  cleanup. **R29-P1 does NOT change this race exposure either
  way** — the per-stop wall is unchanged, the 5 s delay is
  unchanged, only the cross-stop fan-out changed.

  Concurrency-lens conclusion: **no NEW controller-side race
  introduced**. The fix is either (a) push the 5 s default up
  to envelope worst-case c=4 `__fput` (config/operational
  call — TUNING, not RACE) or (b) add a controller-side
  rootfs.img-fd-released probe analogous to
  `wait_for_agent_silent` (driver work — the kernel can't
  surface fcntl-lock state via the agent's HTTP surface
  because the lock is held by CH, not by the agent; this would
  be a host-side filesystem probe, e.g. `flock(fd, LOCK_NB |
  LOCK_EX)` against rootfs.img — but flock semantics differ
  from CH's `O_EXCL`/`fcntl` open).

  **Concurrency does NOT own the kernel-side wedge.** This
  finding documents the controller-side answer to the brief's
  question and shows R29-C1's `.await` change is
  strictly-defensive against this wedge (it doesn't help, but
  it doesn't hurt; and it does close the orthogonal
  R29-C1/r28-C1 leak class).

- **Severity**: MINOR (verification + observation; no defect).
  Documented for the operator-runbook so the c=4 wedge is
  understood to NOT be a controller-side regression from
  R29-C1/R29-P1.

- **Cross-lens**:
  - **arch-r30 owns**: the design call on whether to add a
    controller-side fd-released probe (`flock` against
    rootfs.img / parsing `/proc/locks`). Concurrency-r30 has
    no recommendation — the wedge is DRIVER-side per the brief
    and the architectural fit of a controller-side probe
    against the driver's filesystem state is questionable.
  - **runbook**: operator should treat the c=4 wedge as a
    driver-side `__fput` timing issue and tune
    `vm_index_release_delay_secs` upward if R29-C1/R29-P1
    closure doesn't unblock smoke-r9.

- **Carry mechanism**: NEW for r30; observational only.

### [R30-M2] (NEW MINOR) `snap-idle-gc` loop is the ONLY production `detach_isolated` consumer that does NOT honor `state.shutdown_requested()`

- **Files**:
  - `crates/sandbox/src/registry.rs:1016-1048` — the
    `start_idle_gc` body:
    ```rust
    crate::detach::detach_isolated("snap-idle-gc", move || async move {
        let interval = Duration::from_secs(60);
        ...
        loop {
            compio::time::sleep(interval).await;
            ...
            gc_stop_chunked(&to_kill, &stopper, GC_STOP_CONCURRENCY).await;
        }
    });
    ```
    No `shutdown_requested()` check at the top of the loop OR
    between chunks inside `gc_stop_chunked`.
  - `crates/sandbox/src/sweep.rs:170, 268, 273, 343, 348, 435`
    + `crates/sandbox/src/lib.rs:1433, 1438, 1527, 1535, 1756,
    1764, 1830` — **every other** production
    `detach_isolated` loop checks `shutdown_requested()` at the
    top of each tick + (in some cases) between batched chunks.

- **The shape**:

  R29-P1 inherits the pre-existing absence of the shutdown
  check — the pre-R29-P1 serial loop also didn't check
  `shutdown_requested()`. But R29-P1 makes the per-tick wall
  more variable: 8 parallel stops can wedge on a half-dead
  Nomad agent (`wait_for_job_gone` up to 30 s + host_fence up
  to 120 s + 5 s release delay = up to ~155 s peak wall per
  chunk; under panic, even one stop_one stalling on a TCP
  retry budget pins the whole chunk). During graceful
  shutdown, the controller now potentially holds 8 mid-stop
  futures for up to ~155 s before the GC OS thread exits.
  Compare to the serial pre-fix: one mid-stop future, same
  ~155 s ceiling. So R29-P1 doesn't worsen the wall-time but
  does increase the in-flight-side-effects fan-out during
  shutdown.

  Adding a `shutdown_requested()` check **between chunks**
  inside `gc_stop_chunked` (mirroring `snapshot_rows_chunked`
  at `sweep.rs:712`) would let the GC loop exit cleanly after
  the current chunk completes. Adding a check **at the top of
  the main loop** would let the loop exit cleanly between
  ticks. Both are cheap.

- **Severity**: MINOR (pre-existing, slight amplification
  under R29-P1, no concurrency defect). Production graceful
  shutdown waits for detach_isolated threads to join via
  `detach.rs`'s OS-thread `join` semantics (actually, no —
  `detach_isolated` does NOT track the JoinHandle; the threads
  are truly detached. Process exit just abandons them.). So
  the practical impact is: graceful shutdown either succeeds
  (process tears down before the GC tick completes — Nomad
  cleanup orphans land in the next-boot orphan-prune) OR the
  process holds open for the long Nomad waits. Either way,
  not a concurrency defect.

- **Recommendations** (reviewer-only):

  1. Add `if state.shutdown_requested() { break; }` at the top
     of the `loop` in `start_idle_gc` (above the
     `compio::time::sleep(interval).await`). Lets the loop
     exit between ticks. ~3 LOC.
  2. Add a `shutdown: &dyn Fn() -> bool` parameter to
     `gc_stop_chunked` mirroring `snapshot_rows_chunked`. Check
     between chunks. Lets the loop exit mid-tick on a long
     drain. ~5 LOC.

- **Cross-lens**: arch-r30 / observability-r30 own the
  shutdown-discipline call; concurrency-r30 raises but does
  not own.

- **Carry mechanism**: NEW for r30; pre-existing, deferred
  improvement.

### [R30-M3] (NEW MINOR) `AppStateGcStopper::stop_one`'s `state.sandboxes.get(&id)` call TOUCHES `last_used` on already-expired sandboxes

- **Files**:
  - `crates/sandbox/src/registry.rs:962` — inside `stop_one`:
    ```rust
    if let Some(info) = state.sandboxes.get(&id) {
        tracing::info!(... "sandbox gc: stopping idle sandbox");
    }
    ```
  - `crates/sandbox/src/registry.rs:239-245` — `SandboxRegistry::get`:
    ```rust
    /// Lookup by id; touches `last_used` on hit.
    pub fn get(&self, id: &Uuid) -> Option<SandboxInfo> {
        let guard = self.by_sandbox.read().unwrap();
        let s = guard.get(id)?;
        s.touch();
        Some(s.current_info())
    }
    ```
  - `crates/sandbox/src/registry.rs:584-593` — `expired()`'s
    `idle_for() > idle_threshold` check uses the `last_used`
    field that `get()` just bumped.

- **The shape**:

  Inside the GC loop's stop_one, the `state.sandboxes.get(&id)`
  call BUMPS `last_used` to `Instant::now()` right before
  attempting to stop the sandbox. This is harmless on the
  happy path: the entry is removed at line 983 after stop
  returns. But if `state.backend.stop(id).await` fails (logged
  at 976-980) AND the `state.sandboxes.remove(&id)` panics
  silently inside `catch_unwind` at 982-984, the sandbox's
  `last_used` is now ~`now()` — the next GC tick's `expired()`
  walk will NOT see this sandbox as idle-expired until the
  full `idle_timeout_secs` elapses again.

  Pre-R29-P1 this was a serial-loop concern; R29-P1 makes it
  apply concurrently to 8 sandboxes per chunk — all 8 get
  their idle timers bumped at the same moment if any chunk
  partially fails.

  **The lifetime-expired branch (max_lifetime) is unaffected
  — `lived_for()` reads `created_at` which `touch()` does not
  modify.** So lifetime-expired sandboxes will still be picked
  up. Only idle-expired sandboxes can effectively "hide" from
  the next tick after a partial stop failure.

- **Severity**: MINOR. The failure-mode requires (a) backend.stop
  returns Err, AND (b) the entry isn't removed from the
  registry (which the current code attempts unconditionally
  post-stop, so this is only reachable via the
  `catch_unwind`-suppressed-panic branch). Both branches are
  rare in practice. The pre-existing behaviour: the pre-fix
  serial loop had the same bug. R29-P1 doesn't introduce it
  but does broaden the effect (8 sandboxes per chunk vs. 1).

- **Recommendations** (reviewer-only):

  1. **Replace `state.sandboxes.get(&id)` with a
     touch-free lookup** for the GC log line. The `get()`'s
     touch is documented as intentional for the
     hot-path-tab-open scenario (re-opening a sandbox extends
     its lease); the GC path explicitly does NOT want this
     behaviour. Add a `SandboxRegistry::peek(&self, id: &Uuid)
     -> Option<SandboxInfo>` that omits the `touch()` call,
     route the GC log through it. ~10 LOC + 1 test.

  2. **Alternative**: in `stop_one`, fetch info from the
     `expired()` walk's pre-filtered set rather than
     re-looking-up. The current `expired()` returns
     `Vec<Uuid>`; making it return `Vec<(Uuid, SandboxInfo)>`
     plumbs the info through to `stop_one` without the
     second lookup. ~15 LOC.

  3. **Test oracle** for either: after a `gc_stop_chunked` run
     where `backend.stop(id_0)` returns Err and the registry
     entry persists (pin via a fixture stopper), assert
     `expired()` re-returns `id_0` on the next tick if the
     idle timer was NOT bumped.

- **Cross-lens**: arch-r30 owns the API-shape call (peek vs.
  pre-filtered-info-vec); concurrency-r30 raises the issue
  but it's structurally a code-quality / API-design concern.

- **Carry mechanism**: NEW for r30; pre-existing latent,
  amplified by R29-P1.

## Items NOT findings (verified clean this round)

### [N/A] R29-C1 fix at `62b083e1` — VERIFIED CLOSED

`crates/sandbox/src/backend/nomad_ch.rs:383-404` — new
`release_vm_index_after(...).await` helper. The 5 s
`compio::time::sleep` is bound to the caller's task via `.await`,
not detached, so short-lived `detach_isolated` runtimes (e.g.
the admin snapshot's `snap-teardown-<tail>` path) hold the
runtime alive until the sleep + release completes.

Production call sites verified:

- `stop_inner:1393-1402` (long-lived ntex-worker, snap-idle-gc,
  sweeper; short-lived `snap-teardown-<tail>` via
  `stop_preserving_state` → `stop_inner`). All paths now
  block `stop_inner` on the 5 s release. R29-C1's leak shape
  is closed.
- `CreateGuard::drop:2358-2364` (R28-C1 inline) — refactored
  to use `release_vm_index_after.await` for uniformity with
  the stop_inner site. R28-C1's leak shape is also closed
  (was already closed by the inline `compio::time::sleep` +
  `release()` at the same site).

The new `spawn_delayed_release_in_worker(...) -> Task<...>`
typed escape hatch is unused in production (gated with
`#[allow(dead_code)]`). The typed return value makes the
runtime-lifetime decision explicit at every future call site —
endorses arch-r29-A2 class-fix recommendation. Concurrency-clean.

R29-I1 (the helper-as-footgun observation) is now structurally
addressed: the `pub fn spawn_delayed_release` footgun is
DELETED. The two helpers that replaced it carry their
runtime-lifetime contract in the type (`async fn …` requires
`.await`; `fn … -> Task<…>` requires the caller to decide
`.await` vs `.detach()`). No more silent fire-and-forget against
the current runtime.

### [N/A] R29-P1 fix at `81b6e689` — VERIFIED CONCURRENCY-CLEAN

**Brief-asked properties:**

**(a) panic-safety per-task**: each `stop_one` future's
`backend.stop(id).await` is NOT individually `catch_unwind`-ed
(only the `sandboxes.remove(&id)` tail is). A panic in
`backend.stop` propagates up out of `join_all`, dropping all
sibling futures in the chunk. See R30-I1 — the panic-blast-radius
is structurally amplified vs. the pre-fix serial loop. The
panic-safety property is **observed to be unchanged from
pre-R29-P1 for the per-id execution** (`backend.stop` was
unwrapped in both versions); but the **per-chunk drop-cancel
semantics differ** (1 sandbox lost → up to 8 sandboxes mid-stop
when one panics).

**(b) no shared mutable state across parallel stops**:

- `state.sandboxes` (Arc<SandboxRegistry> internally): each
  `stop_one` calls `state.sandboxes.get(&id)` (acquires a read
  lock briefly + per-sandbox `touch`) then
  `state.sandboxes.remove(&id)` (write lock briefly). Different
  ids → different per-sandbox locks for the touch. The map-level
  read/write locks contend across 8 parallel stops but each
  critical section is bounded to an O(1) hash op. **No race.**
  See R30-M3 for the touch-on-expired-sandbox semantic concern
  (pre-existing; amplified).
- `state.backend.stop(id)` (NomadCHBackend internally): the
  `state.write().remove(&sandbox_id)` at `nomad_ch.rs:1232`
  acquires the per-backend write lock to remove the per-sandbox
  state struct. 8 parallel calls contend on this lock briefly;
  each holds it for an O(1) HashMap remove. **No race.**
- `vm_index_allocator` (Arc<Mutex<VmIndexAllocator>>): every
  parallel `release_vm_index_after` call serialises on this
  Mutex. The critical section is `freed.insert(i)` (BTreeSet
  insert, O(log N)). The `compio::time::sleep(delay)` happens
  OUTSIDE the lock. **No race.** The freed-set is monotonically
  consistent: an `alloc()` interleaving with N parallel
  `release()` calls sees them one-at-a-time (Mutex serialisation),
  each individual release is atomic against alloc. The order of
  releases within a chunk is non-deterministic (per-task wake
  order varies), but `freed: BTreeSet<u16>` is order-independent
  (sorted ascending; smallest is always reused first via
  `freed.iter().next()`).
- Nomad agent state: each `stop_one` issues a `/shutdown` (signed
  POST), a `stop_nomad_job` (unsigned DELETE with purge), a
  `wait_for_job_gone` (polling). These are all per-sandbox-job —
  no shared Nomad-side state across sandboxes. 8 parallel stops
  hit 8 distinct Nomad job IDs. The Nomad-side processing is the
  per-job scheduler's own internal serialisation; from the
  controller's perspective, 8 concurrent HTTP calls are fine
  (Nomad's HTTP API is thread-safe by design).
- `creating_users` set (`HashSet` wrapped in `Mutex`): used
  during `create()` only — `stop` does NOT touch it. **N/A** for
  the GC parallel-stop path.

**Conclusion**: no shared mutable state race introduced by
R29-P1.

**(c) GcStopper trait + impl cancel-safety**:

The `GcStopper` trait's `stop_one` returns
`Pin<Box<dyn Future<Output = Result<(), String>> + 'a>>`. The
future borrows from `&self` for `'a`. In production, the
trait future is consumed by `gc_stop_chunked`'s `join_all` and
awaited to completion. The `snap-idle-gc` loop runs on
`detach_isolated`'s private compio runtime which is NEVER
externally cancelled (no shutdown plumbing into snap-idle-gc;
see R30-M2). So cancel-safety is observed-but-not-load-bearing.

If a future code change DID add cancellation to the GC loop
(e.g. via R30-M2's shutdown check), the cancel semantics are:

- `state.backend.stop(id)` mid-await is cancelled: in-flight
  HTTP request to Nomad is abandoned; the Nomad job purge has
  ALREADY been submitted (the cancel only drops the response
  await), so Nomad will still process the purge eventually.
  vm_index is held by the in-memory state-write before this
  point (`state.write().remove(&sandbox_id)` at `nomad_ch.rs:1232`
  fires synchronously before any await), so the registry-level
  view is consistent. But the `release_vm_index_after` await
  is dropped — vm_index leaks to next-boot orphan-prune. Same
  failure mode as a panic in the per-id stop.

- `release_vm_index_after` mid-sleep is cancelled: the sleep
  future drops; the post-sleep `release(i)` never fires.
  vm_index leaks. **This is the EXACT R29-C1 failure shape on
  a different trigger** — the R29-C1 close ensures the timer
  is bound to the caller's task, but the caller's task can
  still be cancelled externally.

So: cancel-safety of the GC path is observed-clean today, but
the `GcStopper` impl is NOT cancel-safe by structural design.
**Not a defect today** (no external cancellation surface) —
but a CONTRACT to express in the `GcStopper` trait rustdoc if
the trait ever escapes the registry module. See R30-I1
recommendations (1) and (2) — both also close this latent
cancellation defect.

### [N/A] vm_index_allocator's `freed` set under concurrent `release_vm_index_after` callers

The `Arc<Mutex<VmIndexAllocator>>` linearises every `release()`
call. `freed: BTreeSet<u16>` is mutated only under the lock.
The sleep happens OUTSIDE the lock. No interleaving can produce
a torn read or a lost insert. **Verified clean.**

### [N/A] `futures::join!(t5, clock_resync)` cancel-safety (R29-M1 carry-over)

Unchanged from r29. The wake_machine still drives both probes
via `futures::join!` at `wake_machine.rs:529-530`. The
spawn_blocking-based shape is documented cancel-unsafe-but-completes
(compio 0.11 `runtime/mod.rs:208`). Wake_machine runs on
`detach_isolated` with no external cancellation. Clean.

### [N/A] WAKE rootfs.img lock-held wedge (DRIVER-side; brief-asked)

See R30-M1 — the controller does NOT exacerbate the kernel
`__fput` race. R29-C1's `.await` change is strictly defensive
(makes the 5 s settling delay more reliable). R29-P1 does not
touch this path. The wedge fix is DRIVER-side or
operational-config (bumping `vm_index_release_delay_secs`).

## Round-briefing answers (summary)

### 1. R29-P1 — `futures::future::join_all` cap=8 preserves panic-safety per-task?

**Mostly NO** — the pre-R29-P1 serial loop had NO per-task
panic catch around `backend.stop`, and R29-P1 inherits that.
But R29-P1's `join_all` shape AMPLIFIES the panic blast radius
from 1 sandbox mid-stop to up to 8 sandboxes mid-stop per
panicking iteration. See R30-I1 IMPORTANT.

The `sandboxes.remove(&id)` tail IS individually
`catch_unwind`-ed (line 982-984) — that part is preserved
intact and safe.

### 2. R29-P1 — no shared mutable state across the parallel stops would race?

**YES** — verified clean. See `[N/A] R29-P1 fix … VERIFIED
CONCURRENCY-CLEAN` above. The only shared mutable states
(`state.sandboxes` registry, NomadCHBackend's internal state
map, `vm_index_allocator`'s `freed` set, the per-sandbox
`creating_users` set [not touched on stop]) are all
properly-locked at fine granularity; the critical sections are
bounded to O(1) or O(log N) operations.

### 3. R29-P1 — new GcStopper trait/impl pair is cancel-safe?

**Observed-clean** in production today (`snap-idle-gc` has no
external cancellation surface). **Not structurally
cancel-safe** — mid-`backend.stop` cancellation orphans the
Nomad purge response (still processed Nomad-side; controller
loses the response); mid-`release_vm_index_after` sleep
cancellation reproduces the exact R29-C1 leak shape on a
different trigger. If the GC loop ever adds shutdown-check
cancellation (per R30-M2 recommendation), the impl needs
hardening — see R30-I1 (which also covers the panic case via
the same `catch_unwind` mechanism).

### 4. WAKE rootfs.img lock-held wedge — does the CONTROLLER's WAKE state machine exacerbate kernel `__fput`?

**NO new race introduced.** R29-C1 makes the 5 s
`vm_index_release_delay_secs` barrier MORE reliable (was
detached, now `.await`-bound). R29-P1 doesn't touch this
barrier. The wake-side `reserve_vm_index_with_retry` cannot
succeed until the teardown's `release_vm_index_after.await`
has fully elapsed — INCLUDING the 5 s delay.

If 5 s is insufficient for c=4 stress, that's a TUNING
question (bump the config), not a CONCURRENCY race. The
brief's "wake submit fires immediately after snapshot
completes" is true on the API surface (snapshot returns 200
before teardown completes) — but the wake's first phase
(`reserve_vm_index`) BLOCKS on the teardown's release, so the
wake never reaches `submit_restore_job` (the Nomad-job
submission that races kernel cleanup) until AFTER the
controller-side 5 s pause. The kernel `__fput` race is
strictly bounded by the 5 s release delay; if the kernel
needs more, the delay should be raised.

### 5. R29-C1 fix combined with R29-P1 — race conditions in the freed-set update under concurrent `release_vm_index_after` callers?

**NO.** The `Mutex<VmIndexAllocator>` linearises every
`release()` call. The 5 s sleep happens OUTSIDE the lock —
N parallel `release_vm_index_after` futures each sleep
independently, then briefly compete for the mutex to do an
O(log N) `freed.insert(i)`. Mutex contention is bounded and
benign. No torn reads, no lost inserts, no double-release
(`release()` is idempotent against the freed set — a duplicate
insert is a no-op because BTreeSet).

The order in which slots return to `freed` is
non-deterministic across the parallel callers (wake order
varies). The BTreeSet's allocation policy (`freed.iter().next()`
→ smallest) is order-independent, so allocator behaviour is
deterministic-after-release regardless of release order.

## Cross-lens consensus

- **arch-r30 owns**: (1) GcStopper trait per-id-panic-safety
  contract design call (R30-I1); (2) the touch-free
  `SandboxRegistry::peek()` API call (R30-M3); (3) the
  shutdown-discipline call for snap-idle-gc (R30-M2);
  (4) the carry items unchanged (R27-I1, R27-I2, R26-I1,
  R25-I2, R20-I2/I3, R24-I1).
- **test-coverage r30 owns**: (1) the panic-oracle test for
  `gc_stop_chunked` (R30-I1 recommendation 3); (2) the
  touch-on-stop oracle for `AppStateGcStopper::stop_one`
  (R30-M3 recommendation 3); (3) R25-I1 carry.
- **observability-r30 owns**: the R29-M2 fingerprint
  (auto-closed with R29-C1 — confirmed no `host_fence:
  cleared` events SHOULD lack a subsequent `vm_index
  released` in any properly-built post-R29-C1 controller); the
  R28-I2 metric proposal carry; the c=4 wedge runbook
  entry (R30-M1 conclusion: tune `vm_index_release_delay_secs`
  upward if c=4 smoke still wedges post-R29-P1).
- **perf-r30 owns**: R28-DEF1 idle-conn-count interaction
  carry; R26-C1 thread-local-pool teardown cost carry. New
  perf angle from R29-P1: cap=8 fan-out against Nomad's HTTP
  API — 8 concurrent /shutdown + 8 concurrent
  /v1/job/X?purge=true + 8 concurrent /v1/job/X polling
  loops per chunk. The cap-8 ceiling was chosen to avoid
  Nomad-side starvation; whether 8 is the right number is
  a perf measurement, not a concurrency call.
- **code-quality / api-surface r30 owns**: R29-M3
  string-coupling fragility (carry); R27-M1 host_dir_created
  doc-comment carry; R24-I1 fsync_dir doc-misframe carry.
- **stress-r9-retry-6**: with R29-C1 + R29-P1 closed, the
  next stress run should clear the
  `vm-index allocator exhausted` failure mode. If c=4 wedges
  on rootfs.img `__fput` (R30-M1), tune
  `vm_index_release_delay_secs` upward and re-run.

## Carry table

| Finding | Source | r30 state |
|---|---|---|
| R19-C1 wedge-half | r19 → CLOSED r20 | CLOSED carry |
| R20-C1 (Failed side) | r20 → CLOSED r22+r23 e2e | fully CLOSED |
| R20-C1 / R25-I2 (Ok side) | r25 NEW-IMP | **STILL OPEN — PRIORITY FALLS (close at Phase 3)** |
| R20-I1 STATIC_NAMES | r20 → CLOSED | CLOSED carry |
| R20-I2 sweep host-scoping | r20 NEW-IMP | **STILL OPEN — 12th cycle** |
| R20-I3 / R21-I1 / R24-M2 Restoring watchdog | r20→r28 | **STILL OPEN — 12th cycle** |
| R22-I1 observability-gap | r22 → CLOSED r23 | CLOSED carry |
| R22-M3 cross-controller claim | r22 minor | carry |
| R23-I1 (Failed side) | r23 → CLOSED r24 | CLOSED carry |
| R24-I1 fsync_dir doc-misframe | r24 NEW-IMP | **STILL OPEN — code-quality r30 owns** |
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
| R27-I1 host_dir mtime baseline shifts | r27 NEW-IMP | **STILL OPEN — arch-r30 owns** |
| R27-I2 home.img per-user race + Phase 5 latent | r27 NEW-IMP | **STILL OPEN — arch-r30 owns** |
| R27-M1 host_dir_created semantic-overload | r27 minor | carry |
| R27-M2 wait_for_alloc_running budget shift | r27 minor | carry (operator-doc hygiene) |
| R27-M3 R25-I1 sweeper TOCTOU shape SHIFTS | r27 minor | confirmation (no change) |
| R27-DEF1 R26-C1 thread-local Rc<Pool> | r27 defer | CLOSED-with-watchful-eye |
| R28-C1 spawn_delayed_release lost on CreateGuard::drop | r28 NEW-CRIT | **CLOSED at `9e1f6276`** (inline-sleep) + **REINFORCED at `62b083e1`** (refactored to `release_vm_index_after.await` for uniformity) |
| R28-I1 T5 + clock_resync serial→parallel | r28 NEW-IMP | **CLOSED at `d00f12dd`** |
| R28-I2 T5 transport_error structured | r28 NEW-IMP | **CLOSED at `d00f12dd`** |
| R28-M1 verify_agent_version cancel-unsafe by convention | r28 minor | carry (hygiene; no production cancel point) |
| R28-M2 missing log line on CreateGuard::drop release | r28 minor | CLOSED with R28-C1 |
| R28-M3 start_housekeeper concurrency-recheck | r28 minor | CLOSED-with-watchful-eye |
| R28-DEF1 idle-conn count under 500-max-conns | r28 defer | carry (perf-r30 owns) |
| R29-C1 spawn_delayed_release lost on snap-teardown-<tail> | r29 NEW-CRIT | **CLOSED at `62b083e1`** (`release_vm_index_after.await` everywhere) |
| R29-I1 spawn_delayed_release helper is a public footgun | r29 NEW-IMP | **STRUCTURALLY CLOSED at `62b083e1`** (helper DELETED; two typed-contract helpers replace it) |
| R29-M1 futures::join! cancel-safety VERIFIED | r29 minor | CLOSED-with-watchful-eye |
| R29-M2 host_fence:cleared without subsequent release | r29 minor | CLOSED with R29-C1 |
| R29-M3 typed transport_error string-coupling | r29 minor | carry (api-surface / code-quality own) |
| R29-P1 snap-idle-gc serial loop multiplied R29-C1 wall | perf-r29/r30 → r29-P1-fix at `81b6e689` | **CLOSED at `81b6e689`** (verified concurrency-clean here) |
| **R30-I1** `gc_stop_chunked` join_all panic blast amplification | r30 NEW-IMP | NEW |
| **R30-M1** controller has no kernel-__fput barrier beyond 5 s | r30 minor | NEW (verification + observation; brief-answer) |
| **R30-M2** snap-idle-gc loop ignores shutdown_requested() | r30 minor | NEW (pre-existing; amplified by R29-P1) |
| **R30-M3** `state.sandboxes.get(&id)` touches last_used in stop_one | r30 minor | NEW (pre-existing; amplified by R29-P1) |
| Older minors (R19-I2, R19-I3, R20-M1..M5, R21-M1..M3, R22-M1, R23-M1, R24-M5, R25-M2, R25-M3) | older minor | carry |

## Status block

```
Round 30 (R29-C1 + r29-A2 + R29-I1 + R29-P1 CLOSED; new
          IMPORTANT R30-I1 on join_all panic-blast amplification;
          three MINORs covering shutdown-discipline +
          touch-on-stop + WAKE rootfs.img wedge controller-side
          analysis):

  CLOSED THIS ROUND (verified):
    R29-C1 spawn_delayed_release lost on snap-teardown-<tail>
      — CLOSED at 62b083e1 (release_vm_index_after.await
      replaces all production fire-and-forget call sites).
    R29-I1 spawn_delayed_release helper is a public footgun
      — STRUCTURALLY CLOSED at 62b083e1 (helper DELETED;
      replaced by release_vm_index_after [inline-await] and
      spawn_delayed_release_in_worker [typed Task]).
    R29-P1 snap-idle-gc serial-loop wall multiplier
      — CLOSED at 81b6e689 (gc_stop_chunked + cap=8 join_all).
      Verified concurrency-clean against all three brief-asked
      properties (panic-safety per-task: see R30-I1;
      shared-state races: NONE; cancel-safety: observed-clean,
      structurally not-cancel-safe but no external cancel
      surface today).
    R28-C1 reinforced at 62b083e1 (CreateGuard::drop refactored
      to use release_vm_index_after for uniformity; behaviour
      unchanged from r28's inline-sleep close).

  NEW IMPORTANT:
    R30-I1 gc_stop_chunked's join_all amplifies panic blast
      radius from 1 → up to cap=8 mid-stop sandboxes. Today
      no panic path is reachable; structurally the contract
      should be hardened. Recommend per-id catch_unwind in
      AppStateGcStopper::stop_one body (FutureExt::catch_unwind).

  NEW MINOR:
    R30-M1 WAKE rootfs.img wedge (brief-answer): controller
      adds NO kernel-__fput barrier beyond 5 s
      vm_index_release_delay_secs. R29-C1 makes that barrier
      MORE reliable. R29-P1 doesn't touch it. The c=4 wedge is
      DRIVER-side / operational-config (tune the delay up).
    R30-M2 snap-idle-gc loop does NOT honor shutdown_requested().
      Pre-existing; R29-P1's cap-8 fan-out slightly amplifies
      the in-flight side-effects during graceful shutdown.
    R30-M3 AppStateGcStopper::stop_one calls
      state.sandboxes.get(&id) which TOUCHES last_used on
      already-expired sandboxes. Pre-existing; R29-P1 broadens
      effect to 8 sandboxes per chunk. Recommend SandboxRegistry::peek
      for the GC log lookup, or thread info through expired()'s
      return.

  STILL OPEN (carry):
    R20-I2, R20-I3, R24-I1, R25-I1, R25-I2, R26-I1,
    R27-I1, R27-I2, R27-M1, R27-M2.

  ASK:
    (1) arch r30: R30-I1 per-id panic-isolation contract for
        GcStopper trait. Recommend FutureExt::catch_unwind
        in AppStateGcStopper::stop_one body. ~15 LOC.
    (2) arch r30 / api-surface r30: R30-M3 SandboxRegistry::peek
        (touch-free lookup) for the GC log path.
    (3) arch r30: R30-M2 shutdown discipline for snap-idle-gc.
        Cheap fix (~5 LOC).
    (4) test-coverage r30: panic-oracle for gc_stop_chunked
        per R30-I1 recommendation 3. Pin the contract for
        future refactors. ~25 LOC.
    (5) test-coverage r30: touch-on-stop oracle for
        AppStateGcStopper::stop_one (R30-M3 recommendation 3).
    (6) observability / operator-runbook r30: R30-M1 c=4
        wedge messaging — tune vm_index_release_delay_secs
        upward if rootfs.img __fput observations persist
        post-R29-P1. NOT a controller race; do not chase as
        such.
    (7) arch r30 (carry): R27-I1 host_dir mtime baseline;
        R27-I2 home.img per-user race; R26-I1 boot-lookup;
        R25-I2 OK-side terminal-overwrite; R20-I2 sweep
        host-scoping; R20-I3 Restoring watchdog.
    (8) cluster stress-r9-retry-6: with R29-P1 closed, the
        vm-index-exhaustion failure mode should clear. The
        next bottleneck is likely the rootfs.img wedge per
        R30-M1 — tune vm_index_release_delay_secs and re-run.
```

## ASK clarifications for the user

Four open design questions concurrency-r30 raises that need owner
calls before r31:

1. **R30-I1 disposition**: per-id `catch_unwind` in
   `AppStateGcStopper::stop_one`. The `futures::FutureExt::catch_unwind`
   path is the cleanest (wraps the future in `AssertUnwindSafe`
   internally, returns `Result<T, Box<dyn Any + Send>>`).
   Concurrency-r30 recommends this over `compio::runtime::spawn`
   because the GcStopper trait future borrows from `&self` (`'a`
   lifetime) — spawn requires `'static`.

2. **R30-M2 + R30-M3 sequencing**: both are pre-existing latent,
   both are amplified by R29-P1, both are cheap to fix. Suggest
   folding into a single registry-side hardening PR alongside
   R30-I1. ~50 LOC total. Concurrency-r30 endorses.

3. **R30-M1 vs. arch-r30 fd-released probe**: brief-asked angle
   the c=4 WAKE rootfs.img wedge — concurrency-r30 closes
   that the CONTROLLER doesn't exacerbate the kernel `__fput`
   race. If the wedge persists in stress-r9-retry-6, the call
   is between (a) tuning `vm_index_release_delay_secs` upward
   (operational; cheap; addresses 99% of cases) and
   (b) adding a controller-side rootfs.img-fd-released probe
   (architectural; expensive; needs design). Concurrency-r30
   mildly prefers (a) for the cluster-smoke unblock; arch-r30
   owns the (b) call.

4. **Carry items**: R27-I1, R27-I2, R26-I1, R25-I2, R20-I2,
   R20-I3 — all unchanged. arch-r30 owns; user confirm
   whether to fold these into a Phase 5 disposition doc or
   leave as carry through r31.
