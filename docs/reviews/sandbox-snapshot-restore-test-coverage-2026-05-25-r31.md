# Sandbox/snapshot-restore — test-coverage r31 review

Date: 2026-05-25 (UTC). HEAD at audit: `3a53b7ba` (pilot round-44
artifacts; arch r30 inline; `sandbox-snapshot-restore` worktree tip;
clean tree). Round 31. Prior: `docs/reviews/sandbox-snapshot-
restore-test-coverage-2026-05-25-r30.md` (HEAD `e66d5efb`, cycle 43).

Lib test count at HEAD: **549 passed; 0 failed; 1 ignored**
(verified locally via `cargo test -p zeroship-sandbox --lib`).
Was 548 at r30 close — net +1 from `gc_stop_chunked_is_actually_
concurrent` in the R29-P1 GC-parallelize commit `81b6e689`.
Pg-gated unchanged at 94.

## TL;DR

This round answers the four questions in the r31 brief directly.

- **Brief Q1 — R29-P1 GC parallel test pins parallel behavior?**
  The test `gc_stop_chunked_is_actually_concurrent` (registry.rs:850)
  asserts TWO independent properties: (a) wall time < 5 s (serial
  would be ≥ 18 s), and (b) `max_in_flight == cap` (= 8). The second
  assertion pins parallel behavior at the cap boundary specifically,
  NOT just "faster than serial." The design deliberate: n=10 over
  cap=8 produces two chunks (8, 2), and the first chunk at per_stop=2s
  fires all 8 futures into the same sleep simultaneously, so
  `max_in_flight` reaches cap under any sane scheduler. The test
  satisfies the R29-P1 regression-pin mandate. **One gap remains**:
  the second chunk (size=2) is asserted only via wall time; no
  `min_in_flight_second_chunk` assert exists. This is acceptable (the
  second-chunk fan-out is self-evidently correct from the first-chunk
  proof), but see **[R31-T1] MINOR** below.

- **Brief Q2 — `release_vm_index_after` coverage across production
  call sites.** Two production call sites exist in-crate:
  `stop_inner:1393` (the stop/snap-teardown path) and
  `CreateGuard::drop:2358` (the create-rollback path). The R29-C1
  regression test at `:4882` exercises the helper directly through
  `detach_isolated` — it covers the runtime-lifetime property
  (runtime-lifetime axis) but not either call-chain. The R28-C1
  regression test at `:4670` exercises `CreateGuard::drop` → the
  production `detach_isolated("create-rollbk")` call site — it
  covers call-site 2. **Call-site 1 (`stop_inner`) has no
  integration-level test** that traces the full chain from the HTTP
  endpoint through `teardown_source_for_snapshot → stop_preserving_
  state → stop_inner → release_vm_index_after`. This is the
  unchanged R30-T1 gap. See carry table below.

- **Brief Q3 — architecture r30-A4 missing concurrency tests
  (rootfs.img inode lock, host_dir prefix create-race, tap-iface
  table). Effort + priority.** r30-A4 names a docs/architecture/
  concurrency-hazards.md hazard map as an IMP item. The three
  hazards named in the brief correspond to three open test gaps
  surfaced by r30-A2 and earlier arch findings. These are detailed
  in **[R31-A1] NEW IMPORTANT** below with effort estimates and
  layer classification.

- **Brief Q4 — cluster-level surprises: map gaps to test layer.**
  Each cluster-peel finding from the T8b stress series maps to a
  "no controller-side test would have caught this" statement. The
  map is in **[R31-G1] gap-analysis** below.

- **TOTAL NEW r31 IMPORTANT items: 1 (R31-A1). NEW MINOR items:
  1 (R31-T1).** All carries from r30 unchanged unless noted.

---

## CRITICAL

None.

---

## IMPORTANT

### [R31-A1] [NEW] Architecture r30-A4 named three concrete
concurrency-test gaps — effort map and priority

The arch-r30 inline commit (`3a53b7ba`) names r30-A4 as:
"docs/architecture/concurrency-hazards.md hazard map + missing
tests (~300 LOC)." The three hazards the r31 brief asks about
are each distinct in kind and test-layer home.

#### Hazard 1: rootfs.img inode lock (r30-A2)

**Background** (arch r30 inline, cycle 44): the wake restore path
hardlinks rootfs from a shared template. CH's `--restore` mode
takes an OFD write lock on `rootfs.img`. When two concurrent wakes
share the same template inode (hardlink, not COW), the second wake
fails immediately with "Can't get Write lock for rootfs.img as
there is already a ExclusiveWrite lock." This is exactly the c=4
smoke result: WAKE 0/8 at `stage=resume`, driver v20
`start_task_restore_failures_total{stage="resume"} = 8/8`. The
r30-A2 fix is driver-side: `cp --reflink=always` in v22 so each
wake gets a unique COW inode.

**Test-layer home**: CROSS-WORKTREE (driver-side). The property
"each wake gets a unique inode" is only observable in the driver's
`bake-restore-disk.sh` or equivalent staging step. A controller-
side test cannot observe inodes on the host filesystem.

**Current coverage**: driver v21 added a typed counter
(`wake_rootfs_lock_wait`) and bounded-fail defense (165 tests).
v22's COW fix (dispatched in cycle 44) should add a unit test
in the driver worktree that asserts reflink output has a
different inode than the template. That is the right assertion
to land; it belongs in the driver test suite, not here.

**Effort estimate**: ~20 LOC driver-side. **Priority: P0 (blocks
c>1 cluster).**

**Controller-side gap**: none. There is no controller call that
controls the rootfs hardlink/COW choice. The gap is driver-side;
this crate cannot cover it.

#### Hazard 2: host_dir prefix create-race

**Background**: `NomadCHBackend::try_create` calls
`std::fs::create_dir_all(&host_dir_owned)` at `nomad_ch.rs:922`
to create the per-sandbox directory under the shared
`cfg.nomad_ch.host_path` prefix. If two concurrent creates race on
the SAME sandbox_id (which the per-user serialization gate at
`:678-691` prevents by returning 409 for the second), or if the
prefix itself does not exist yet, `create_dir_all` can race with
a concurrent `rm -rf` from the sweeper's `host-dir-gc` loop
(`sweep.rs:1252`).

**Current coverage**: `stop_for_real_leaks_host_dir_for_sweeper`
(nomad_ch.rs:7349) covers the GC-vs-host_dir lifecycle contract
at the unit level (single-threaded). **No test fires concurrent
`try_create` + `spawn_host_dir_gc` against the same prefix.**

The per-user create-serialization gate means the most dangerous
race (two creates for the same UUID) is already gated — the
higher-risk race is `try_create` for UUID A landing in the same
`host_path` root as a concurrent `rm -rf` of UUID B's parent
that was mis-scoped. Looking at the sweeper's GC logic in
`sweep.rs:1252`, it removes specific UUIDs; the shared prefix is
never removed. The residual race is:

1. Worker 1: `create_dir_all(prefix/uuid-a)` — prefix exists, ok.
2. Worker 2: `rm -rf(prefix/uuid-b)` at the same moment.
3. Both succeed because they touch different leaf dirs.

This race is benign at the filesystem level (disjoint paths). The
ACTUAL dangerous race is **create_dir_all of uuid-A simultaneously
with a prior in-flight workspace.img mkfs.ext4 in uuid-A's dir**
— but that's prevented by the per-user gate, not by filesystem
atomics.

**Test-layer home**: unit-level, no Postgres needed. A
`#[test]` (not `#[compio::test]`) that spawns N threads each
calling `create_dir_all(tmpdir/uuid-N)` concurrently, with a
separate thread calling `fs::remove_dir_all(tmpdir/uuid-M)`, and
asserts both sides observe their expected outcome. ~30 LOC.

**Priority**: LOW. The directory isolation (per-UUID paths, no
shared leaf) means the race is structural benign at the fs level.
The prior cluster bugs were not filesystem race bugs; they were
Nomad job + VM state races. Defer until arch r31 closes
r30-A4's docs item, at which point the hazard map will clarify
whether this race is in-scope or out-of-scope for coverage.

**Effort estimate**: ~30 LOC, no pg-gate needed.

#### Hazard 3: tap-iface table (vm_index allocation race)

**Background**: `VmIndexAllocator` (`nomad_ch.rs:301-460`) is an
in-memory `BTreeSet<u16>` (freed set) + monotone cursor behind
`Arc<Mutex<VmIndexAllocator>>`. Each `alloc()` takes the mutex,
removes the min of freed (or advances the cursor), returns a u16.
The tap device name `zsbx-nm-<vm_index>` is derived deterministically
from this u16. Two concurrent creates for DIFFERENT users each
call `alloc()` — since allocation is mutex-serialized, no two
callers can get the same index. The per-user gate additionally
prevents the SAME user from racing two allocations.

**The residual hazard** is the window between `alloc()` and the
kernel tap-device bind in the Nomad wrapper script. If a
previous sandbox's job is still dying (wrapper exiting, kernel
releasing the tap device), and a fresh `alloc()` returns the
same index (because `release_vm_index_after` already ran), a
new Nomad job can race the old wrapper for `tap=zsbx-nm-<idx>`.
This is the `wait_for_job_gone` + r24-A2-S3 release-delay design:
the 5 s delay keeps the slot "dirty" until the kernel cleans up.

**Current coverage**: The allocator has 7 unit tests covering
`alloc`/`release`/`reserve` semantics and the idempotent double-
release at `:4730`. The `wait_for_job_gone`-then-release ordering
is covered at `stop_preserving_state_does_not_remove_host_dir`
(`:7277`) and `restored_sandbox_is_stoppable_and_releases_vm_index`
(`:7349`). **No multi-threaded / concurrent-alloc test exists that
fires two goroutines at `alloc()` simultaneously** to verify the
mutex property holds under parallel stress.

**What the r30 pilot commit says** about this (concurrency r30):
"R29-P1 verified clean (no races introduced; freed-set linearised
by `Mutex<VmIndexAllocator>`)." This is a structural-argument
verification, not an executable property.

**What WOULD close it** (~25 LOC, no pg-gate):

```rust
#[test]
fn vm_index_alloc_is_mutex_linearized_under_parallel_stress() {
    // N threads each alloc() then release() M times;
    // assert no index is ever held by two threads simultaneously
    // (observable as: the set of currently-allocated indices,
    // tracked via a second Mutex<HashSet<u16>>, never has a
    // duplicate).
    let pool = Arc::new(Mutex::new(VmIndexAllocator::new(1, 10)));
    let in_use = Arc::new(Mutex::new(std::collections::HashSet::new()));
    let dup_detected = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let threads: Vec<_> = (0..8).map(|_| {
        let pool = Arc::clone(&pool);
        let in_use = Arc::clone(&in_use);
        let dup = Arc::clone(&dup_detected);
        std::thread::spawn(move || {
            for _ in 0..100 {
                if let Ok(i) = pool.lock().unwrap().alloc() {
                    let inserted = in_use.lock().unwrap().insert(i);
                    if !inserted { dup.store(true, Ordering::SeqCst); }
                    std::thread::yield_now();
                    in_use.lock().unwrap().remove(&i);
                    pool.lock().unwrap().release(i);
                }
            }
        })
    }).collect();
    for t in threads { t.join().unwrap(); }
    assert!(
        !dup_detected.load(Ordering::SeqCst),
        "VmIndexAllocator: duplicate allocation under parallel stress — mutex not linearizing"
    );
}
```

**Test-layer home**: unit-level, no pg-gate, no compio runtime
needed. A plain `#[test]`.

**Priority**: MEDIUM. The Mutex guarantee is sound by Rust type
system, but the test pins it as an executable property and would
catch any future refactor that replaces `Mutex` with a lock-free
structure that introduces an ABA window. The r29-P1 concurrency
reviewer noted the freed-set is "linearised by `Mutex`" but did
not add the property as a test. ~25 LOC; fast.

**Effort estimate**: ~25 LOC, no pg-gate.

#### Summary table

| Hazard | Layer | Pg-gate | Effort | Priority | Status |
|---|---|---|---|---|---|
| rootfs.img inode lock | CROSS-WORKTREE (driver) | No | ~20 LOC | P0 | Driver v22 COW fix; test in driver worktree |
| host_dir prefix create-race | Unit | No | ~30 LOC | LOW | Structural benign; defer to r30-A4 hazard-map closure |
| tap-iface table (alloc race) | Unit | No | ~25 LOC | MEDIUM | **[R31-T1]** below; no test pins mutex property today |

---

## MINOR

### [R31-T1] [NEW] R29-P1 GC parallel test asserts `max_in_flight
== cap` but only for the FIRST chunk; second chunk (size=2) has
no semantic overlap assert

**Where**: `crates/sandbox/src/registry.rs:908-917`

```rust
// (b) Semantic check — under any sane scheduler all `cap`
// futures of the first chunk reach the sleep before any
// wakes, so max_in_flight == cap.
assert_eq!(
    max_overlap, cap,
    "gc_stop_chunked must overlap within a chunk: ..."
);
```

**What it does**: `n=10`, `cap=8` → two chunks: first chunk 8
futures (all reach `compio::time::sleep` before any wake →
`max_in_flight` reaches 8), second chunk 2 futures. The
`max_in_flight` tracking records the peak across BOTH chunks.
Since the first chunk drives `max_in_flight` to 8, the
`assert_eq!(max_overlap, cap)` passes even if the second chunk
somehow serialized.

**What it does not pin**: whether `gc_stop_chunked` correctly
waits for the second chunk to complete before returning. A
regression that truncates after the first chunk (e.g., a
`chunks(cap)` bug that skips remainder-sized final chunks)
would leave `n - cap = 2` ids unprocessed. The wall-time bound
(assert `elapsed < 5 s`) catches truncation only if the second
chunk's work ALSO adds wall time — which it does (2 × 2 s ≈ 4
s total makes the `< 5 s` bound tight). So the wall-time bound
implicitly covers the second chunk, but not via a semantic
oracle.

**What WOULD close it** (~5 LOC addition):

```rust
// After gc_stop_chunked returns:
let total_processed = ...; // count via the stopper
assert_eq!(total_processed, n,
    "gc_stop_chunked must process ALL ids, not just the first chunk");
```

This requires the `SleepingGcStopper` to also count total calls
(an `AtomicUsize` initialized to 0, incremented at entry to
`stop_one`). Then a final `assert_eq!(calls.load(...), n)`.

**Severity**: MINOR. The wall-time bound (claim (a)) does cover
the second chunk indirectly. The semantic check (claim (b))
explicitly documents "under any sane scheduler all `cap` futures
of the first chunk reach the sleep" — this is only true for the
FIRST chunk; the comment is subtly misleading about what the
assert actually pins (peak across both chunks = first-chunk peak,
since 8 > 2). The risk is a future reader misreading the comment
as "we verified each chunk". ~5 LOC to add a `stop_count`
AtomicUsize and the final `assert_eq!(stop_count, n)`.

---

### [R31-T2] [NEW] `VmIndexAllocator` mutex-linearization under
parallel stress has no executable test

This is Hazard 3 from [R31-A1] extracted as a standalone minor.
The concurrency r30 reviewer noted the freed-set is "linearised
by `Mutex<VmIndexAllocator>`" but did not add an executable
property test. ~25 LOC, no pg-gate, `#[test]` (no compio needed).
See [R31-A1] §Hazard 3 for the test shape. **Severity MINOR.**

---

### Carries unchanged from r30

| Tag | r30 status | r31 status | Notes |
|-----|------------|------------|-------|
| R30-T1 snap-teardown admin call-chain | NEW IMPORTANT r30 | **IMPORTANT (2nd round)** | ~80 LOC pg-gated. Highest-leverage carry. |
| R29-T2 T5 drive() integration | IMPORTANT (2nd round) | **IMPORTANT (3rd round)** | ~120 LOC pg-gated. |
| R29-T3 staging-skip contract | IMPORTANT (2nd round) | **IMPORTANT (3rd round)** | ~50 LOC. |
| R28-T2 verbatim-msg exit | IMPORTANT (9th carry) | **IMPORTANT (10th carry)** | No demotion. |
| R27-T3 boot-failure composition | IMPORTANT (5th carry) | **IMPORTANT (6th carry)** | ~30 LOC. |
| R30-T2 futures::join! cancel-safety | MINOR r30 | **MINOR (2nd round)** | ~40 LOC. Optional. |
| R30-T3 wake_machine half-dead rollback | MINOR r30 | **MINOR (2nd round)** | ~60 LOC pg-gated. |
| R29-T4 BackendBuilder unit tests | MINOR (carry) | **MINOR (carry)** | ~40 LOC. |
| R29-T5 release-log emission | MINOR (carry, optional) | **MINOR (carry, optional)** | ~25 LOC. |
| R29-T1 housekeeper rationale | MINOR (doc-only carry) | **MINOR (doc-only carry)** | ~5 LOC. |
| R27-T6-LIB sweep orchestration | MINOR (4th carry) | **MINOR (5th carry)** | ~30 LOC pg-gated. |
| R27-T4 read_snapshot_row pg | MINOR (3rd carry) | **MINOR (4th carry)** | ~40 LOC pg-gated. |
| R28-S1 sanitize bare-UUID | MINOR carry | **MINOR (carry)** | Defer until Phase 2 stress. |
| r1-DISC-2 transport-flake | MINOR carry (optional) | **MINOR (carry, optional)** | ~30 LOC. |
| R22-T3 retry-race pg | MINOR (10th carry) | **MINOR (11th carry)** | ~80 LOC. |
| R25-T1 stress harness | OPEN (6th) | **OPEN (7th); cross-worktree** | ~120 LOC. |
| R25-T3 vm_index race | OPEN (6th) | **OPEN (7th); cross-worktree** | narrowed by r24-A2-S3 + R29-A2. |
| R28-T3 ext4 magic | CROSS-WORKTREE | unchanged | Driver-side. |
| R28-T5 wire-schema parity | CROSS-WORKTREE | unchanged | Pending Phase 3. |
| R28-T8 prod-state driver fixture | CROSS-WORKTREE | unchanged | Tracked by driver worktree. |

---

## [R31-G1] Gap analysis — cluster-level surprises and the test
layer that would catch them

The r31 brief asks: "each cluster peel was a 'no test would have
caught this' finding — map gaps to existing test layer."

The T8b stress series findings map as follows:

### Mapping table

| Cluster finding | HEAD when surfaced | Root cause class | Test layer that WOULD catch it | Currently exists? | Gap tag |
|---|---|---|---|---|---|
| stress-r9-retry-4: 394/400 CREATE fast-fail, `vm-index allocator exhausted (floor=1, ceil=12)` | `508c3d76` (pre-R29-C1) | vm_index LEAK: `detach_isolated("snap-teardown-…")` → `stop_inner` → `spawn_delayed_release` fired on a short-lived runtime; timer dropped on runtime drop | Controller unit test: `#[test]` (not `#[compio::test]`) firing `detach_isolated` + polling allocator | YES — landed as `release_vm_index_after_survives_short_lived_runtime` (`:4882`) at `62b083e1` AFTER the cluster | Admin call-chain integration: R30-T1 (admin handler → stop_inner path) NOT YET covered |
| stress-r9-retry-5: 0/400 CREATE, `vm-index allocator exhausted` WITHIN FIRST TICK | `81b6e689` (R29-C1 fixed, GC not-yet-parallelised) | Serialized GC loop: N × 5 s wall per tick (N=12 → 60 s ≥ tick interval → allocator never drained) | Controller unit test: `gc_stop_chunked_is_actually_concurrent` (`:850`) — asserts wall < 5 s and max_in_flight == cap | YES — landed at `81b6e689` which is also the commit that CAUSED the observable symptom at retry-5 shape | Wall-time bound would catch regression to serial in next run |
| stress-r9-retry-6: 0/400 CREATE (same), single-VM smoke c=1 GREEN | `0ee106d2` | Cluster shape: c=20 exhausts cap=12 in one tick regardless of GC speed. NOT a GC issue | Cluster harness shape mismatch: unit tests can't cover this; correct fix is reducing c in the harness OR raising ceil | N/A — harness design issue, not controller-code defect | Not a test gap; a cluster-configuration gap |
| c=4 smoke, WAKE 0/8 at stage=resume | `0ee106d2` | rootfs.img OFD inode collision: hardlink → shared inode → CH `--restore` ExclusiveWrite lock contention | Driver-side: test that COW (`cp --reflink=always`) output has distinct inode from template | NO (driver v22 dispatched) | R28-T3 class — CROSS-WORKTREE |
| stress-r9-retry-4 LEAK signal MASKED by harness exhaustion | `508c3d76` | Test-oracle failure: cluster with c=20 vs. cap=12 saturates allocator on cycle 1; leak contribution indistinguishable from harness-induced contention | Controller unit test: the R29-C1 regression test at `:4882`. A controller-unit with single-slot allocator catches the leak with c=1, regardless of harness shape | NO at the time; YES after `62b083e1` | R30-G1 from last round — documented there |

### Pattern statement

Every cluster surprise in the T8b series falls into one of three
classes:

1. **Controller-unit detectable at c=1** (vm_index leak, GC
   serialization): unit tests with a single-slot allocator and
   wall-time bound catch these. The cluster is the WRONG oracle —
   c=20 exhausts the allocator before leaks are distinguishable.
   Clusters are useful for load-shape regressions but not for
   single-item leak detection.

2. **Cross-worktree detectable** (rootfs inode, driver
   staging): the controller has no call site to cover; the property
   belongs in the driver test suite. These findings confirm the
   r30-A4 mandate: the concurrency-hazards doc should enumerate
   which hazards are controller-side vs driver-side so future
   cluster failures can be triaged faster.

3. **Harness configuration** (c=20 vs. cap=12): not a code defect.
   No test covers this; the right response is adjusting the stress
   harness to `c ≤ ceil` (or to a c-ramp shape that observes
   allocator saturation separately from leak contribution).

### What r31 adds to this picture

r30-A2 (rootfs.img inode, driver-side) and r30-A4 (concurrency-
hazards map) together close the gap on class 2. The missing piece
for class 1 is **R30-T1** (the admin-call-chain integration test)
and **[R31-T2]** (mutex-linearization stress test) — both are
controller-unit level, no cluster needed, and both would have
caught their respective bugs before a cluster run.

---

## Brief Q1 — Detailed verdict on `gc_stop_chunked_is_actually_concurrent`

**Does the test ACTUALLY pin parallel behavior, or just at cap?**

It pins parallel behavior in the first chunk with high fidelity.
The design parameters are: `n=10`, `cap=8`, `per_stop=2s`.

Chunk decomposition: `ids.chunks(8)` → `[8 ids][2 ids]`.

First chunk: `futures::future::join_all` drives all 8 stop_one
futures onto the single compio task. Each future immediately
calls `fetch_add(in_flight, SeqCst)`, records the peak via
`fetch_max(max_in_flight, SeqCst)`, then sleeps. Because all 8
are co-resident on the same compio task (single-threaded join_all,
not multi-threaded), all 8 are driven to the yield point (the
`.await` on `compio::time::sleep`) before any of them wakes.
This guarantees `max_in_flight` reaches 8.

Second chunk: 2 futures. The wall-time bound (< 5 s total) forces
the second chunk to complete, but the `max_in_flight` remains 8
(from chunk 1). There is no explicit `max_in_flight` == 2 assert
for the second chunk. The wall bound covers completion, not
overlap cardinality. This is the gap that **[R31-T1]** surfaces.

**Verdict**: the test pins parallel behavior at cap for the FIRST
chunk unconditionally (structural, not probabilistic). The second
chunk is covered by wall-time implication only. The test is
correct and useful; the minor gap is the missing `stop_count`
total-processed assert. The test WOULD catch any regression to
serial execution (because a serial regression would produce
`max_in_flight == 1` and `elapsed ≥ 18 s`).

---

## Brief Q2 — `release_vm_index_after` production call site coverage

| Call site | File:line | Context | Test coverage |
|---|---|---|---|
| `stop_inner` (stop + snap-teardown path) | `nomad_ch.rs:1393` | Called after `wait_for_job_gone` succeeds; inline-await on the shared ntex runtime or the `detach_isolated("snap-teardown-…")` private runtime | Helper level: `release_vm_index_after_survives_short_lived_runtime` (`:4882`) — pins runtime-lifetime property. Admin call-chain: **NONE** (R30-T1 gap). |
| `CreateGuard::drop` (create-rollback path) | `nomad_ch.rs:2358` | Inside `detach_isolated("create-rollbk", …)`; called after purge confirms | `create_guard_drop_releases_vm_index_under_isolated_runtime` (`:4670`) — pins the full CreateGuard → detach_isolated → release_vm_index_after chain. COVERED. |

**Summary**: CreateGuard::drop (R28-C1 site) has a test that
exercises the full production call-chain. stop_inner (R29-C1 site)
has a test that exercises the helper directly through
`detach_isolated` but not the 4-hop chain. The admin-handler
integration test (R30-T1) is the open gap for the second site.

---

## Phase 4 cutover gate sufficiency

**INSUFFICIENT** (unchanged from r30) without:

- **[R30-T1]** admin-snapshot detach-chain integration
  (IMPORTANT, 2nd round).
- **R29-T2** T5 drive() integration (IMPORTANT, 3rd round).
- **R29-T3** staging-skip contract (IMPORTANT, 3rd round).
- **R28-T1** Phase 3 manifest validation (cross-worktree pending).
- **R28-T2** verbatim-msg exit (10th carry).
- **R28-T3** ext4 magic (cross-worktree).
- **R28-T5** wire-schema parity (cross-worktree).
- **R27-T3** boot-failure composition (6th carry).

8 asks; same count as r30 (no new IMPORTANT in r31 for this
gate). The NEW r31 IMPORTANT (R31-A1) contains the rootfs inode
(cross-worktree, driver v22) and the mutex-linearization test
(MEDIUM, deferrable). Neither blocks Phase 4 cutover.

LOC budget for IN-WORKTREE Phase-4 gates: **~80 (R30-T1)** +
120 (R29-T2) + 50 (R29-T3) + 35 (R28-T2 pg) + 40 (R28-T2 admin)
+ 30 (R27-T3) = **~355 LOC controller-side**. ~700 LOC total
including cross-worktree driver items.

---

## To test-cov r32 backlog (~420 LOC controller-side)

1. **R30-T1** — admin-snapshot detach-chain integration.
   **~80 LOC pg-gated. IMPORTANT (2nd round).** Highest-leverage
   carry.
2. **R29-T2** — T5 drive() integration. ~120 LOC pg-gated.
   **IMPORTANT (3rd round).**
3. **R29-T3** — staging-skip contract. ~50 LOC. **IMPORTANT
   (3rd round).**
4. **R28-T2 / R26-T1** verbatim-msg exit (10th carry). ~75 LOC.
   **IMPORTANT.**
5. **R27-T3** boot-failure composition. ~30 LOC.
   **IMPORTANT (6th carry).**
6. **[R31-T1]** gc_stop_chunked total-processed assert. ~5 LOC.
   **MINOR (NEW r31).** Trivial; bundle with next registry.rs edit.
7. **[R31-T2]** `VmIndexAllocator` mutex-linearization stress.
   ~25 LOC. **MINOR (NEW r31).**
8. **R30-T2** — futures::join! cancel-safety pin. ~40 LOC.
   **MINOR (2nd round).** Optional.
9. **R30-T3** — wake_machine half-dead rollback drive() integration.
   ~60 LOC pg-gated. **MINOR (2nd round).** Bundle with R29-T2.
10. **R29-T4** BackendBuilder unit tests. ~40 LOC. **MINOR (carry).**
11. **R29-T5** release-log emission. ~25 LOC. **MINOR (carry,
    optional).**
12. **R29-T1** housekeeper rationale fix. ~5 LOC docstring.
    **MINOR (doc-only carry).**
13. **R27-T6-LIB** sweep orchestration. ~30 LOC. **MINOR (5th
    carry).**
14. **R27-T4** read_snapshot_row pg-gated. ~40 LOC. **MINOR
    (4th carry).**
15. **R28-S1** sanitize bare-UUID widening. ~10 LOC. **MINOR;
    defer until Phase 2 stress.**
16. **r1-DISC-2** transport-flake variants. ~30 LOC. **MINOR
    (carry, optional).**
17. **R22-T3** retry-race pg (11th carry). ~80 LOC. **MINOR.**

---

## Delta accounting

Lib test count: **549** (r30 baseline 548, +1 net).

| Commit | What landed | Tests added |
|---|---|---|
| `81b6e689` (R29-P1 GC parallelize) | `gc_stop_chunked` + `GcStopper` trait + `AppStateGcStopper` + `start_idle_gc` wiring | +1 (`gc_stop_chunked_is_actually_concurrent`) |
| `9ac5b850` (R28-API2 sweep) | `cfg(any(test, feature = "test-support"))` gates on 4 pub test items; deletion of `set_role_dsns_for_test` | 0 |
| `3a53b7ba` (pilot r44 artifacts + inline arch r30) | reviewer-artifact + inline notes, no in-crate source | 0 |

Net: +1. The 549 → r31 increment is the single R29-P1 GC test.
Pg-gated at 94 (unchanged from r30).

---

## Notes for r32

- **r31 is a focused brief-answer round.** Four brief questions
  answered; two new minor items ([R31-T1], [R31-T2]) surfaced.
  Neither is a Phase-4 gate blocker.

- **The pattern from r30 continues**: the largest open gaps are
  all INTEGRATION-LAYER items (R30-T1, R29-T2, R29-T3) where
  predicates are unit-tested but the outer dispatch system that
  INTERPRETS the predicate output is not exercised. The pattern
  has now been found four times (T5 / half-dead-agent /
  release_vm_index_after helper / admin-chain dispatch). The rule:
  for every new predicate that gates control flow in a wider
  state machine or HTTP dispatch chain, land BOTH a predicate
  unit test AND an integration test.

- **r30-A2 (rootfs inode COW)** is a P0 driver-side item (v22).
  The controller cannot cover it. r31's analysis confirms this;
  the concurrency-hazards doc (r30-A4) should make this cross-
  worktree attribution explicit so future triage is faster.

- **Cluster oracle sufficiency**: the mapping in [R31-G1] confirms
  that ALL four T8b controller-code defects were detectable at
  c=1 with a single-slot allocator. The cluster adds confidence
  about harness-shape regressions but is the wrong oracle for
  controller-unit leak bugs. The implication for r32: before
  scheduling a new stress cluster run, confirm the R30-T1 /
  R29-T2 / R29-T3 integration tests are green. A cluster run
  that fails due to an uncovered code defect costs real dollars
  and cycle time; the controller-unit tests are the cheaper
  oracle.

- **Build state**: `cargo test -p zeroship-sandbox --lib` 549
  passed / 0 failed / 1 ignored at HEAD `3a53b7ba`, verified
  locally. Two pre-existing warnings (restore.rs:43, sweep.rs:96)
  unchanged.

- **Backlog cardinality**: 17 items at r32 entry; net +2 from r31
  ([R31-T1], [R31-T2]), both minor. R31-A1's rootfs inode item
  is cross-worktree (driver v22) and does not add to the
  controller-side backlog. The controller-side LOC budget is
  ~420, unchanged from r30's ~395 plus the two new 5+25 LOC
  items.
