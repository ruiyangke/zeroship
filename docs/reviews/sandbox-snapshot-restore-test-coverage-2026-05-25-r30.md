# Sandbox/snapshot-restore — test-coverage r30 review

Date: 2026-05-25 (UTC). HEAD at audit: `e66d5efb` (controller
pin bump v36→v37; `sandbox-snapshot-restore` worktree tip;
clean tree). Round 30. Prior: `docs/reviews/sandbox-snapshot-
restore-test-coverage-2026-05-25-r29.md` (HEAD `5a0647c3`).
Companion cluster: `docs/reviews/sandbox-snapshot-restore-
cluster-2026-05-25-T8b-stress-r9-retry-4.md`.

Lib test count at HEAD: **548 passed; 0 failed; 1 ignored**
(verified locally; was 540 at r29 close — net +8 from the
R29-C1 class-fix at `62b083e1` and the R28-I1+I2 T5
parallelize at `d00f12dd`; see §"Delta accounting" below).
Pg-gated unchanged at 94.

## TL;DR

- **The brief asks five tightly-scoped questions; the answers
  are concrete.** This round is the verification round on
  R29-C1 (class-fix landed) and R28-I1+I2 (T5 parallelize +
  half-dead-agent detection), plus a gap-analysis on the
  stress-r9-retry-4 cluster run — `508c3d76`-vintage,
  pre-R29-C1-fix. The cluster surfaced a vm-index allocator
  exhaustion (394/400 fast-fail) that overwhelmed any leak
  signal at that HEAD; the R29-C1 leak was active but
  masked. r30's new finds answer the brief's five questions
  directly and surface ONE NEW IMPORTANT gap on the same
  pattern as R29-T2: predicate has unit tests, integration
  layer has none.

- **Brief Q1 — snap-teardown admin path leak-fix pin.** The
  R29-C1 regression test at `nomad_ch.rs:4882` is named
  `release_vm_index_after_survives_short_lived_runtime` and
  exercises the HELPER through `detach_isolated`, NOT the
  admin handler call chain. The actual production-leak path
  is `admin_handlers.rs:1491 detach_isolated("snap-teardown-
  <tail>") → teardown_source_for_snapshot →
  stop_preserving_state → stop_inner`. **No integration
  test exists that fires the admin endpoint and asserts the
  vm_index slot is back in `freed` after the detached
  teardown.** See **[R30-T1] NEW IMPORTANT** below.

- **Brief Q2 — R28-I1+I2 cancel-safety pin.** The commit
  message claims cancel-safety as a property of the
  `futures::join!(t5_future, clock_resync_future)` pair:
  "both futures take args by value or as borrows whose
  lifetimes span the join; no shared mutable state, no Drop-
  side effects." The 5 added tests at `restore_handler.rs`
  pin OUTCOME SHAPE (Match / Mismatch / Skipped + transport-
  error flags) and PARALLEL ARITY (call counter = 2). **No
  test exercises cancel-safety as an executable property** —
  e.g., dropping the joined future mid-flight and asserting
  no allocator-leak, no half-stale agent_url reservation,
  no panic surfaces in the dropped task. See **[R30-T2]
  NEW MINOR**.

- **Brief Q3 — R28-DISCIPLINE compliance of the R29-C1
  test.** The test DOES reproduce the production lifecycle
  in the load-bearing dimension (R28-DISCIPLINE-compliant
  on the runtime-lifetime axis): `#[test]` (not
  `#[compio::test]`), plain `std::thread`, invokes
  `crate::detach::detach_isolated("test-r29-c1", …)` which
  mints a fresh `compio::runtime::Runtime::new()` +
  `block_on(fut)` then drops — identical to the
  admin-handler dispatch shape. The 100ms delay is
  load-bearing (delay > runtime-Ready cycle). **Verdict:
  COMPLIANT.** What it does NOT reproduce is the *call-
  chain* (no teardown_source_for_snapshot → stop_inner
  call); the helper-level fixture is what R28-DISCIPLINE
  asks for (production-state oracle = real `detach_isolated
  ` runtime; the production-path traversal is the
  integration-test concern, which IS the [R30-T1] gap).

- **Brief Q4 — stress-r9-retry-4 gap analysis.** The
  cluster ran at `508c3d76` (pre-R29-C1-fix). 394/400
  CREATEs fast-failed with `vm-index allocator exhausted
  (floor=1, ceil=12)` from c=20 vs ceiling=12 + 5s release
  delay. The R29-C1 LEAK signal was MASKED: every
  snap-teardown was leaking a slot, but c=20 saturated the
  allocator on the FIRST cycle anyway, so the leak
  contribution was indistinguishable from harness-induced
  contention. **A controller-side unit test would have
  caught R29-C1 BEFORE the cluster**; the cluster is the
  wrong oracle for this class of leak. See **[R30-G1]
  gap-analysis** below. The R29-C1 regression test at
  `:4882` is exactly that test, landed AFTER the cluster
  evidence — the order was inverted.

- **Brief Q5 — architecture suggestion: predicate test
  using a synthetic short-lived compio Runtime.** This is
  what the landed `release_vm_index_after_survives_short_
  lived_runtime` already does. The brief's suggestion is
  SATISFIED by the landed test; the open architectural
  question is whether to surface a public test-helper
  `compio_runtime_lifetime::with_short_lived_runtime(|rt|
  ...)` for re-use by other detached-future fixtures (TBD
  scope: at least 4 detached-future call sites exist —
  CreateGuard::drop, snap-teardown, wake-machine drive,
  admin_post_wake). See **[R30-A1] OPTIONAL** below.

- **TOTAL NEW r30 IMPORTANT items: 1 (R30-T1). NEW MINOR
  items: 3 (R30-T2, R30-T3, R30-A1).** All carries from
  r29 unchanged unless noted.

## CRITICAL

None.

## IMPORTANT

### [R30-T1] [NEW] snap-teardown admin-call-path integration test missing — the R29-C1 SIBLING site is unpinned

**Where**: `crates/sandbox/src/admin_handlers.rs:1491-1506`
— the `detach_isolated("snap-teardown-{tail}", ...)`
invocation that fires after a successful admin
`/admin/sandboxes/{id}/snapshot` POST.

**Production-call-chain**:

```
admin_handlers::snapshot_sandbox (HTTP handler)
  → snapshot_handler::snapshot_sandbox(...).await        // returns 200
  → detach_isolated("snap-teardown-<tail>", ||
      async {
        backend.teardown_source_for_snapshot(sandbox_id).await
          → NomadCh::teardown_source_for_snapshot
            → NomadCh::stop_preserving_state
              → NomadCh::stop_inner(.., remove_host_dir = false)
                → VmIndexAllocator::release_vm_index_after(...) // R29-A2 inline-await
      });
```

**Coverage today** (post-R29-C1-fix at `62b083e1`):

| Layer | Test | File:line | Reproduces production-detach? |
|---|---|---|---|
| Helper | `release_vm_index_after_survives_short_lived_runtime` | `nomad_ch.rs:4882` | YES (runtime-lifetime axis); NO (call-chain) |
| Helper | `create_guard_drop_releases_vm_index_under_isolated_runtime` | `nomad_ch.rs:4669` | YES (CreateGuard::drop sibling) |
| Helper | `release_vm_index_after_honors_configured_delay` | `nomad_ch.rs:4782` | NO (compio::test) |
| Helper | `spawn_delayed_release_in_worker_returns_joinable_task` | `nomad_ch.rs:4820` | NO (compio::test) |
| Backend | `teardown_source_for_snapshot_preserves_host_dir_then_stop_reaps` | `sandbox_pg_e2e.rs:3533` | NO — `#[compio::test]`; calls `backend.teardown_source_for_snapshot(sid).await` DIRECTLY from the worker runtime (NOT through detach_isolated) |
| Admin | `admin_ro_bearer_rejected_on_write_endpoint_with_403` etc. | `sandbox_admin_e2e.rs:873-1100` | N/A — these test AUTH preflight (501/503/403/401), no payload reaches the detach_isolated call site |

**The gap**: NO test fires the HTTP endpoint, observes the
detached teardown, and asserts the allocator slot is back
in `freed` after the runtime drops. The R29-C1 test pins
the HELPER (`release_vm_index_after`) under
`detach_isolated`; the admin-handler call-chain that
INVOKES this helper through 4 layers of dispatch is
uncovered.

**Why it matters**:
- A refactor that reverts ANY ONE of the 4 dispatch hops
  back to a `compio::runtime::spawn(...).detach()` shape
  (e.g., "let's parallelize teardown_source_for_snapshot by
  detaching the host_fence sleep") would NOT trip the
  helper-level R29-C1 regression test. The helper is fine;
  the call-chain is reverted.
- The R29-C1 bug fired BECAUSE the runtime-lifetime
  mismatch happened MID-CHAIN. The class-fix pinned the
  helper, but the dispatch chain itself is the load-
  bearing seam.
- The cluster oracle (stress-r9-retry-4) couldn't see the
  leak because of harness-shape exhaustion. Without [R30-
  T1] the next refactor that re-introduces a mid-chain
  `.detach()` will not be caught until the cluster shape
  ALSO surfaces it — which the c=20 harness specifically
  does NOT.

**What WOULD close it** (~80 LOC, pg-gated):

```rust
#[compio::test]
#[ignore = "needs Postgres; admin /snapshot detach-chain end-to-end"]
async fn admin_snapshot_endpoint_releases_vm_index_via_detached_teardown() {
    // 1. Build state with snapshot wiring + a real (test) Database +
    //    a NomadCh backend whose VmIndexAllocator is single-slot
    //    (floor=ceil=N) so the leak is observable as
    //    `alloc().is_err()` after the detached path completes.
    let db = migrated_db().await;
    let token = "admin-bearer-..";
    let mut cfg = sweep_test_cfg(true);
    cfg.nomad_ch.vm_index_floor = 88;
    cfg.nomad_ch.vm_index_ceil = 88;
    cfg.nomad_ch.vm_index_release_delay_secs = 0; // collapse to inline
    // (or set to 1 to actually exercise the delay path)
    let state = make_state_with_snapshot_wiring_and_backend(
        Some(token.into()), db, cfg,
    );
    let svc = make_app!(state);

    // 2. Inject a sandbox at vm_index=88 (so alloc is exhausted).
    //    The pre-handler state is alloc()-fail.
    let sid = inject_running_sandbox(&state, 88).await;

    // 3. Fire POST /admin/.../snapshot. Wait for 200.
    let req = test::TestRequest::default()
        .method(POST)
        .uri(&format!("/admin/sandboxes/{sid}/snapshot"))
        .header("authorization", &format!("Bearer {token}"))
        .to_request();
    let resp = test::call_service(&svc, req).await;
    assert_eq!(resp.status(), 200);

    // 4. The detached teardown thread is now running on its
    //    private compio runtime. Poll up to 10 s for the
    //    allocator slot to come back. If the pre-R29-C1-fix
    //    detach shape regressed, this never observes.
    let allocator = state.backend_nomad_ch_test_handle().vm_index_allocator();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut released = false;
    while Instant::now() < deadline {
        if let Ok(i) = allocator.lock().unwrap().alloc() {
            assert_eq!(i, 88);
            released = true;
            break;
        }
        compio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        released,
        "R29-C1 admin-path regression: detached snap-teardown \
         did not release vm_index — runtime-lifetime mismatch \
         in the dispatch chain"
    );
}
```

Needs a `backend_nomad_ch_test_handle()` accessor on
state (a `#[cfg(test_support)]`-gated reach-through to
`VmIndexAllocator`) plus the existing `_test_inject_
sandbox` lib helper. Both are already in tree.

**R28-DISCIPLINE relevance**: the helper-level R29-C1 test
satisfies R28-DISCIPLINE for the predicate (real
`detach_isolated`, real runtime drop, real timer). r30-T1
applies R28-DISCIPLINE to the INTEGRATION layer (the
admin handler is the production caller; the cluster is
the production oracle). Exact same shape as R29-T2's
"predicate has unit tests, drive() has none."

**Severity IMPORTANT (NEW).** ~80 LOC pg-gated. Land
before the next admin-handler refactor or detach-chain
restructure.

## MINOR

### [R30-T2] [NEW] R28-I1+I2 cancel-safety pin — futures::join! drop-mid-flight property unverified

**Where**: `crates/sandbox/src/wake_machine.rs:124-126` —
the `futures::join!(t5_future, clock_resync_future)` pair.

**Commit-message claim** (at `d00f12dd`):

> "Cancel-safety: both futures take args by value or as
> borrows whose lifetimes span the join; no shared mutable
> state, no Drop-side effects. The outer task awaits the
> join to completion, so even hypothetical cancellation
> can't tear them mid-call."

This is a structural argument, not an executable property.
The 5 R28-I1+I2 tests at `restore_handler.rs:360-660` pin:

1. `clock_resync_typed_surfaces_transport_error_on_closed_port`
2. `clock_resync_typed_non_200_is_not_transport_error`
3. `parallel_t5_and_clock_resync_both_succeed_against_healthy_agent`
4. `half_dead_agent_fingerprint_detected_when_both_probes_transport_fail`
5. `parallel_asymmetric_failure_does_not_trip_half_dead_agent`

All pin OUTCOME SHAPE (Match / Mismatch / Skipped +
transport_error flag) and PARALLEL ARITY (call counter
= 2). **None drop the joined future mid-flight** to
verify the cancel-safety claim.

**What's NOT tested**:
- Dropping the `futures::join!(...)`-returned future
  before it completes leaves no allocator state behind.
- The spawn_blocking-wrapped ureq calls don't surface
  panics into the outer task on drop.
- The wake_machine.drive() outer task, if dropped between
  livez and register, leaves the wake_job row in a
  consistent state (no half-completed transitions).

**What WOULD close it** (~40 LOC):

```rust
#[ntex::test]
async fn parallel_join_dropped_midflight_leaves_no_state() {
    // Same fixture as parallel_t5_and_clock_resync_both_succeed,
    // but use futures::FutureExt::now_or_never() OR a select!
    // against a 0-ms timeout to force a drop BEFORE both probes
    // complete.
    let (agent_url, calls) = spawn_fake_agent(...);
    let t5 = verify_agent_version_post_restore(...);
    let cr = clock_resync_post_restore_typed(...);
    let joined = futures::future::join(t5, cr);
    drop(joined); // never polled — no allocator side-effects expected.
    // Sleep a tick to let any spurious spawn_blocking complete.
    compio::time::sleep(Duration::from_millis(50)).await;
    // Assertion: no panics propagated, no test allocator
    // touched (no allocator in this fixture; the property
    // is "drop does not panic the test thread").
    assert!(calls.load(AOrdering::SeqCst) <= 2,
        "drop must not cause additional calls beyond the polled prefix");
}
```

**Severity MINOR (NEW).** ~40 LOC. The structural argument
is sound (no shared mutable state in the futures); this
test pins it as an executable property. Defer unless the
wake_machine acquires shared mutable state (allocator
reservation mid-livez, etc.) — at which point this becomes
IMPORTANT.

### [R30-T3] [NEW] wake_machine half-dead-agent rollback path has no drive()-level test

**Where**: `crates/sandbox/src/wake_machine.rs:546-588` —
the half-dead-agent detection branch:

```rust
if t5_transport_error && clock_resync_transport_error {
    tracing::warn!(target: "sandbox::wake::half_dead_agent", ...);
    return self.rollback_with(
        g1, snap.vm_index,
        WakeErrorCode::ClockResyncFailed,
        format!("half_dead_agent: ..."),
    ).await;
}
```

**Coverage today**: 1 unit test pins the FINGERPRINT
detection at the restore_handler layer (`half_dead_agent_
fingerprint_detected_when_both_probes_transport_fail` at
`restore_handler.rs:582`), asserting the typed-boolean
contract on the OUTCOMES. **No drive()-level test pins:**

- The wake_machine ACTUALLY ENTERS the half-dead branch
  when both outcomes report transport_error.
- The branch INVOKES `rollback_with(WakeErrorCode::Clock
  ResyncFailed, "half_dead_agent:" prefix)`.
- The branch DOES NOT FIRE for the single-transport-error
  case (asymmetric_failure test at restore_handler covers
  the OUTCOME pair, not the wake_machine's interpretation).
- The branch correctly SHORT-CIRCUITS — i.e., does NOT
  enter the `verify_agent_version_post_restore` outcome
  switch BELOW the half-dead branch (a refactor that
  swaps the branch order could regress this silently).

This is the same R28-DISCIPLINE pattern R29-T2 surfaced
for T5's drive() integration — the predicate has unit
tests, the state machine's interpretation of the predicate
output is uncovered.

**What WOULD close it** (~60 LOC, pg-gated; same fixture
shape as R29-T2):

```rust
#[compio::test]
#[ignore = "needs Postgres; half-dead-agent rollback e2e"]
async fn wake_machine_drive_rolls_back_on_half_dead_agent() {
    // Seed: snapshotted row + sealed Persistence.
    // Build: WakeMachine WITH persist=Some(_).
    // Stub backend: livez stub returns Ok (port answered),
    //   then BOTH /version and /_clock_resync time out
    //   (bind+drop fake-agent shape — accepts livez, refuses
    //   subsequent signed-GETs).
    // Expectation:
    //   - wake_job row in state=Failed
    //   - error_code = "clock_resync_failed"
    //   - error_message starts with "half_dead_agent:"
    //   - sandbox row reverts to Snapshotted
    //   - allocator slot is released
}
```

**Severity MINOR (NEW).** ~60 LOC pg-gated. Sibling of
R29-T2; the same `spawn_fake_agent` helper applies. Bundle
with the R29-T2 implementation round (single fixture file
covers both).

### [R30-A1] [OPTIONAL] Test-helper opportunity: shared `with_short_lived_runtime`

**Where**: `crates/sandbox/src/backend/nomad_ch.rs:4669`
(R28-C1 regression) and `:4882` (R29-C1 regression) are
nearly identical shapes:

```rust
#[test]
fn ..._under_isolated_runtime() {
    let pool = Arc::new(Mutex::new(VmIndexAllocator::new(N, N)));
    let allocated = pool.lock().unwrap().alloc().expect("alloc");
    // ... build CreateGuard OR call detach_isolated ...
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut released = false;
    while Instant::now() < deadline {
        if let Ok(i) = pool.lock().unwrap().alloc() {
            assert_eq!(i, N);
            released = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(released, "...");
}
```

The polling loop + allocator-as-leak-oracle pattern is now
duplicated. A test-helper:

```rust
#[cfg(test)]
fn assert_slot_released_within<F: FnOnce(Arc<Mutex<VmIndexAllocator>>, u16)>(
    floor_ceil: u16,
    deadline: Duration,
    trigger: F,
) {
    let pool = Arc::new(Mutex::new(VmIndexAllocator::new(floor_ceil, floor_ceil)));
    let slot = pool.lock().unwrap().alloc().expect("alloc");
    trigger(Arc::clone(&pool), slot);
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        if pool.lock().unwrap().alloc().is_ok() { return; }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("slot {slot} not released within {deadline:?}");
}
```

Would collapse both R28-C1 and R29-C1 regression tests
(~50 LOC each) into a 3-line invocation each. ALSO makes
[R30-T1] (admin-path integration) a 5-line test instead
of 80.

**Severity OPTIONAL.** Not a coverage gap — a refactor
opportunity that PREEMPTS future divergence between the
two regression tests (they're already DIFFERENT in subtle
ways: R28-C1 uses CreateGuard::new directly, R29-C1 uses
detach_isolated directly; the polling logic is bit-for-bit
identical).

### Carries unchanged from r29

| Tag | r29 status | r30 status | Notes |
|-----|------------|------------|-------|
| R29-T2 T5 drive() integration | NEW IMPORTANT | **IMPORTANT (2nd round)** | ~120 LOC pg-gated. Highest-leverage carry. |
| R29-T3 staging-skip contract | NEW IMPORTANT | **IMPORTANT (2nd round)** | ~50 LOC. Pair with Phase 4 default-flip. |
| R29-T4 BackendBuilder | NEW MINOR | **MINOR (carry)** | ~40 LOC. |
| R29-T5 release-log emission | NEW MINOR | **MINOR (carry, optional)** | ~25 LOC. |
| R29-T1 housekeeper rationale | NEW MINOR | **MINOR (doc-only carry)** | ~5 LOC docstring fix. |
| R28-T2 verbatim-msg exit | IMPORTANT (8th carry) | **IMPORTANT (9th carry)** | No demotion. |
| R27-T3 boot-failure composition | IMPORTANT (4th carry) | **IMPORTANT (5th carry)** | ~30 LOC. |
| R27-T6-LIB sweep orchestration | MINOR (3rd carry) | **MINOR (4th carry)** | ~30 LOC pg-gated. |
| R27-T4 read_snapshot_row pg | MINOR (2nd carry) | **MINOR (3rd carry)** | ~40 LOC pg-gated. |
| R28-S1 sanitize bare-UUID | MINOR carry | **MINOR (carry)** | Defer until Phase 2 stress. |
| r1-DISC-2 transport-flake | MINOR carry | **MINOR (carry, optional)** | ~30 LOC. |
| R22-T3 retry-race pg | MINOR (9th carry) | **MINOR (10th carry)** | ~80 LOC. |
| R25-T1 stress harness | OPEN (5th) | **OPEN (6th); cross-worktree** | ~120 LOC. |
| R25-T3 vm_index race | OPEN (5th, indirectly mitigated) | **OPEN (6th); cross-worktree** | r24-A2-S3 + R29-A2 narrow it further. |
| R28-T3 ext4 magic | CROSS-WORKTREE | unchanged | Driver-side. |
| R28-T5 wire-schema parity | CROSS-WORKTREE | unchanged | Pending Phase 3. |
| R28-T8 prod-state driver fixture | CROSS-WORKTREE | unchanged | Tracked by driver worktree. |

## [R30-G1] Gap analysis — would a controller-side test have caught R29-C1 BEFORE stress-r9-retry-4?

**Brief Q4 verbatim**: "what test would have caught the
R29-C1 leak BEFORE cluster surfaced it?"

**Answer**: A controller-side `#[test]` (NOT
`#[compio::test]`) that dispatches a non-zero-delay
release through `detach_isolated` and polls the allocator
for the freed slot — the EXACT shape of the landed R29-C1
regression test at `nomad_ch.rs:4882`. The R28-C1
regression test at `:4669` (different call site — CreateGuard
::drop, NOT stop_inner) HAD ALREADY LANDED at r28; the gap
was that nobody audited the OTHER call sites of
`spawn_delayed_release` for the same runtime-lifetime
mismatch class.

**Actual sequence**:

| Round | What happened | What was missing |
|---|---|---|
| r28 (cycle ~38-40) | R28-C1 discovered + fixed at `9e1f6276` (inline-await in `CreateGuard::drop`). Test added at `:4669`. | Audit of OTHER `spawn_delayed_release` callers. The `stop_inner` call site at `:1377` was NOT inspected for the same runtime-lifetime mismatch class, despite being reachable from `detach_isolated("snap-teardown-...", ...)`. |
| r29 (cycle 40-41) | concurrency-r29 reviewer surfaced R29-C1: `admin_handlers.rs:1491 detach_isolated("snap-teardown-{tail}", ...) → teardown_source_for_snapshot → stop_preserving_state → stop_inner → spawn_delayed_release` — same runtime-lifetime bug, second site. | Class-fix: convert `spawn_delayed_release` to an `async fn release_vm_index_after` that AWAITS inline. Landed at `62b083e1`. |
| Cluster stress-r9-retry-4 (2026-05-24 23:01 UTC, HEAD `508c3d76`) | 394/400 fast-fail with vm-index allocator exhaustion. CREATE-side failure masked the R29-C1 LEAK signal. The 6 successful CREATEs went through fine; the SUBSEQUENT detached snap-teardowns leaked their slots BUT c=20 already saturated the allocator on cycle 1. | Cluster oracle unable to distinguish "harness exhausted allocator" from "R29-C1 leak exhausted allocator". |
| r30 (this round) | Verifies the class-fix landed + asks whether the admin call-CHAIN is integration-tested (it isn't — [R30-T1]). | [R30-T1] admin-path integration test. |

**The discipline lesson**: r28's R28-C1 fix correctly
identified the CreateGuard::drop site, but the round did
not enumerate ALL callers of `spawn_delayed_release` to
audit for the same class. r29's R29-C1 class-fix did the
enumeration retrospectively and chose to DELETE
`spawn_delayed_release` entirely — replacing it with the
typed `release_vm_index_after` (inline-await, safe
everywhere) + `spawn_delayed_release_in_worker` (typed
Task, forces caller to confront runtime lifetime). This
is the correct class-fix shape; the gap was the round
between r28 and r29 where the second site silently
shipped.

**A test that would have surfaced R29-C1 at r28**: a
sweep-style audit test that enumerates all callers of
`spawn_delayed_release` and asserts each is on a
long-lived runtime. This is hard to express as a Rust
test (call-graph analysis), so the practical equivalent
is **a `cargo grep` pre-merge hook** that flags
`spawn_delayed_release` callers AND `detach_isolated`
callers and requires explicit annotation that the
runtime-lifetime question has been considered. This is a
process-discipline ask, not a test-coverage ask;
documented here for r30 closure.

**Brief Q5 — architecture suggestion**: predicate test
using synthetic short-lived compio runtime. **The landed
R29-C1 test at `:4882` IS this test.** The `#[test]`
plain-thread + `detach_isolated` shape mints a synthetic
short-lived runtime (fresh `compio::runtime::Runtime::
new()` + `block_on` + drop) that reproduces the
production lifecycle. Verdict: BRIEF Q5 SATISFIED by
the landed test. The OPEN question is whether to
generalise the fixture into a shared `with_short_lived_
runtime` helper — see **[R30-A1] OPTIONAL** above.

## Phase 4 cutover gate sufficiency

**INSUFFICIENT** (unchanged from r29) without:

- **[R30-T1]** admin-snapshot detach-chain integration
  (NEW IMPORTANT this round).
- **R29-T2** T5 drive() integration (2nd round carry).
- **R29-T3** staging-skip contract (2nd round carry).
- **R28-T1** Phase 3 manifest validation (cross-worktree
  pending).
- **R28-T2** verbatim-msg exit (9th carry).
- **R28-T3** ext4 magic (cross-worktree).
- **R28-T5** wire-schema parity (cross-worktree).
- **R27-T3** boot-failure composition (5th carry).

8 asks; 1 NEW for r30; rest carry/cross-worktree.

LOC budget for IN-WORKTREE Phase-4 gates: **~80 LOC
(R30-T1)** + 120 (R29-T2) + 50 (R29-T3) + 35 (R28-T2 pg)
+ 40 (R28-T2 admin) + 30 (R27-T3) = **~355 LOC
controller-side**. ~700 LOC total including driver-side
cross-worktree work.

## To test-cov r31 backlog (~395 LOC controller-side)

1. **R30-T1** — admin-snapshot detach-chain integration.
   **~80 LOC pg-gated. IMPORTANT (NEW r30).** Highest-
   leverage new r30 item. Closes the R29-C1 admin-side
   gap that the helper-level regression test does not.
2. **R29-T2** — T5 drive() integration. ~120 LOC pg-
   gated. **IMPORTANT (2nd round).** Highest-leverage
   carry.
3. **R29-T3** — staging-skip contract. ~50 LOC.
   **IMPORTANT (2nd round).** Pair with Phase 4 default-
   flip.
4. **R28-T2 / R26-T1** verbatim-msg exit (9th carry).
   ~75 LOC. **IMPORTANT.**
5. **R27-T3** boot-failure composition. ~30 LOC.
   **IMPORTANT (5th carry).**
6. **R30-T2** — futures::join! cancel-safety pin. ~40 LOC.
   **MINOR (NEW r30).** Optional unless wake_machine
   acquires shared mutable state.
7. **R30-T3** — wake_machine half-dead-agent rollback
   drive() integration. ~60 LOC pg-gated. **MINOR
   (NEW r30).** Bundle with R29-T2 fixture.
8. **R29-T4** BackendBuilder unit tests. ~40 LOC.
   **MINOR (carry).**
9. **R29-T5** release-log emission. ~25 LOC. **MINOR
   (carry, optional).**
10. **R30-A1** `with_short_lived_runtime` test helper.
    ~30 LOC refactor. **OPTIONAL.**
11. **R29-T1** housekeeper rationale fix. ~5 LOC
    docstring. **MINOR (doc-only carry).**
12. **R28-T6** sweep creator-agnostic. ~15 LOC. **MINOR
    (carry).**
13. **R27-T6-LIB** sweep orchestration. ~30 LOC. **MINOR
    (4th carry).**
14. **R27-T4** read_snapshot_row pg-gated. ~40 LOC.
    **MINOR (3rd carry).**
15. **R28-S1** sanitize bare-UUID widening. ~10 LOC.
    **MINOR; defer until Phase 2 stress.**
16. **r1-DISC-2** transport-flake variants. ~30 LOC.
    **MINOR (carry, optional).**
17. **R22-T3** retry-race pg (10th carry). ~80 LOC.
    **MINOR.**

## Delta accounting

Lib test count: **548** (r29 baseline 540, +8 net).
Breakdown:

| Commit | What landed | Tests added | Tests adapted/renamed |
|---|---|---|---|
| `62b083e1` (R29-C1 class-fix) | `release_vm_index_after` inline-await + `spawn_delayed_release_in_worker` typed Task | +4 (zero-delay, configured-delay, joinable-task, short-lived-runtime regression) | 2 renamed (the original `spawn_delayed_release_with_zero_delay_releases_immediately` and `spawn_delayed_release_honors_configured_delay`); net +2 from `spawn_delayed_release_in_worker_returns_joinable_task` and `release_vm_index_after_survives_short_lived_runtime` |
| `d00f12dd` (R28-I1+I2 T5 parallelize) | `futures::join!(t5, clock_resync)` + half-dead-agent fingerprint + `ClockResyncOutcome` enum | +5 (clock_resync_typed_surfaces_transport_error_on_closed_port, clock_resync_typed_non_200_is_not_transport_error, parallel_t5_and_clock_resync_both_succeed_against_healthy_agent, half_dead_agent_fingerprint_detected_when_both_probes_transport_fail, parallel_asymmetric_failure_does_not_trip_half_dead_agent); plus existing T5 Skipped tests updated to assert on `transport_error` field | 0 |
| Other (`b8310356` heredoc audit, `680baafa` driver pin bump, paperwork) | — | 0 | 0 |

Net: +2 (R29-C1) + 5 (R28-I1+I2) + 1 unclassified = +8.
The 1 unclassified is most likely an existing test
ADAPTED into pinning the new `transport_error` field
shape — non-load-bearing for the count.

Pg-gated unchanged at 94 (r1-DISC-3's 3 tests landed at
r29; no pg-gated tests landed in r30 commits).

## Notes for r31

- **r30 is the verification round** on r29's two big
  in-worktree landings (R29-C1 class-fix + R28-I1+I2 T5
  parallelize). Both passed the discipline-audit: the
  R29-C1 helper-level test IS R28-DISCIPLINE-compliant
  (real `detach_isolated`, real runtime drop, non-zero
  delay) and the R28-I1+I2 tests pin the typed boolean
  contract (transport_error) as a STRUCTURAL signal not a
  string-match.

- **The pattern that surfaced again**: R28-DISCIPLINE
  applied at the integration layer (the rule R29-T2
  generalises). Three NEW r30/r29 items fit this pattern:

  | Predicate has unit tests | Integration is uncovered |
  |---|---|
  | T5 verify_agent_version_post_restore (8 tests at restore_handler:4182) | wake_machine.drive() (0 tests with persist=Some) — **R29-T2** |
  | Phase 2 `driver_stages_disk_images` JSON emit (3 tests at nomad_ch:7657) | controller-side staging bypass (0 tests) — **R29-T3** |
  | `release_vm_index_after` helper (4 tests at nomad_ch:4756-4905) | admin /snapshot detach-chain (0 tests) — **R30-T1** |
  | Half-dead-agent OUTCOME pair (1 test at restore_handler:582) | wake_machine.drive() rollback branch (0 tests) — **R30-T3** |

  Four instances now. The rule is generalisable: **for
  every NEW predicate that gates control flow in a wider
  state machine OR HTTP dispatch chain, land BOTH a unit
  test of the predicate AND an integration test of the
  outer system's interpretation of the predicate's
  outcomes.**

- **Cluster-vs-controller-test triangulation**: stress-
  r9-retry-4 ran at the pre-R29-C1-fix HEAD and FAILED to
  surface the leak — the harness shape masked it. This is
  a recurring pattern: clusters are the wrong oracle for
  controller-side leak bugs. The R29-C1 helper-level
  regression test would have caught this at r28 if the
  audit had enumerated all `spawn_delayed_release`
  callers. r30 surfaces the same gap one level up: the
  helper-level test still doesn't cover the admin call-
  chain (R30-T1).

- **No emoji, no celebratory framing.** r30 is a closure-
  validation round + a one-new-finding round. R29's
  forward-looking gates (R29-T2, R29-T3) carry; r30 adds
  R30-T1 (admin-chain integration) as the highest-
  leverage new ask. Net direction: r30 continues r29's
  shift toward INTEGRATION-LAYER coverage gates while r28
  closed the PREDICATE-LAYER discipline question.

- **Build state**: `cargo test -p zeroship-sandbox --lib`
  548 passed / 0 failed / 1 ignored at HEAD `e66d5efb`,
  verified locally. Two pre-existing warnings (restore.
  rs:43, sweep.rs:96) unchanged.

- **Backlog cardinality**: 17 items at r31 entry; net +1
  from r30 (R30-T1 NEW IMPORTANT). 3 NEW MINOR (R30-T2,
  R30-T3, R30-A1) are optional-deferrable. The
  controller-side LOC budget is ~395, well within a 2-
  round drain at the historical 100-200 LOC/round pace.
