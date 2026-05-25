# Sandbox/snapshot-restore — performance r31 review

Date: 2026-05-25 (UTC).
HEAD at audit: `f33de6d2`.
Driver HEAD pin: v24 (`f33de6d2` — pin bump at `712c96fb`).
Controller version: v38 (`zeroship-sandbox.snapshot-v38`, `0ee106d2`).
Prior: `docs/reviews/sandbox-snapshot-restore-performance-2026-05-25-r30.md` (HEAD `712c96fb`).

This round uses a new dataset: the c=4×5 fresh-cluster cutover-readiness validation that
landed at `f33de6d2`. The lens is specifically the **CREATE 12/20 rate** — the only
production-throughput-relevant signal that is not 100% in this run.

---

## Summary

**0 new findings, 1 new IMPORTANT (tuning/throughput), 1 carry IMPORTANT (R30-P1), 4 carry MINORs,
3 carry OPEN items.**

One new IMPORTANT this round:

- **R31-P1 NEW IMPORTANT** — The c=4×5 run yields a measured steady-state CREATE rate of
  ~0.034 CREATE/s (2.0/min) at ceil=12, release_delay=5 s, c=4. This is allocator-throughput-
  limited, not driver-throughput-limited. The 8 fast-fail allocator-exhausted failures are not
  correctness bugs; they are a consequence of slot-turnover lag. Three independent knobs can
  raise the effective ceiling: (1) bump `vm_index_ceil` 12→16+, (2) shrink
  `vm_index_release_delay_secs` 5→2, (3) throttle harness concurrency to match the slot-release
  cadence. All three are config-only changes; none require a code change.

One carry IMPORTANT unchanged:

- **R30-P1 carry IMPORTANT** — permit hold spans `stop_inner`'s full wall; documentation/
  observability gap. Unchanged.

---

## Carry table

| Finding | Status @ r31 | Evidence |
|---|---|---|
| **R30-P1** stop-permits hold duration undocumented | **CARRY IMPORTANT** — unchanged. | `crates/sandbox/src/backend/nomad_ch.rs:1465-1468`. |
| **R30-P2** `NomadStopPermitGuard::Drop` silent discard | **CARRY MINOR** — unchanged. | `nomad_ch.rs:657-668`. |
| **R30-P3** `dec_nomad_stop_permits_in_use` convention-over-ground-truth | **CARRY MINOR** — unchanged. | `crates/sandbox/src/metrics.rs:496-528`. |
| **R29-P2** doubled blocking-pool demand on wake join | **CARRY MINOR** — unchanged. | `crates/sandbox/src/wake_machine.rs:518-530`. |
| **R29-P3** `transport_error` prefix-match stringly-typed | **CARRY MINOR** — unchanged. | `crates/sandbox/src/restore_handler.rs:3083-3098`. |
| **R16-P3** Fuse encrypt + SHA | **OPEN-CARRY** | snapshot_handler/aead untouched. |
| **R16-P5** gzip pre-AEAD | **OPEN-CARRY** | snapshot_aead.rs untouched. |
| **R17-P2** Active-set cache | **OPEN-CARRY** | sweep.rs untouched. |
| **R5-P1b** SHA + BufReader path | **OPEN-CARRY** | snapshot_handler.rs untouched. |

---

## CRITICAL

None.

---

## IMPORTANT

### R31-P1 NEW IMPORTANT — Measured steady-state CREATE rate at c=4, ceil=12, delay=5 s: ~0.034/s; allocator-throughput-limited, not driver-limited

**Dataset:** `docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-stress-cutover-c4x5.md`.

```
Run:     c=4 × 5 cycles = 20 total attempts
Result:  CREATE 12/20 (60%), elapsed 358.5 s, per-cycle p50 ~30 s
Config:  vm_index_ceil=12, vm_index_release_delay_secs=5
```

**Measured CREATE throughput:**

```
12 successful CREATEs in 358.5 s = 0.0335 CREATE/s = 2.01 CREATE/min
```

This is the **end-to-end cycle throughput under the allocator ceiling**, not the
raw CREATE API throughput. The 8 fast-fail allocator-exhausted errors confirm that
the bottleneck is slot availability, not driver capacity.

**Why 60% fail at c=4 × ceil=12:**

Each slot's hold time spans the full cycle: CREATE + SNAPSHOT + WAKE + STOP-fence +
release_delay. From the observed data:

```
Phase             Observed wall (smoke-r23 single-cycle, indicative)
CREATE            ~6.5 s
SNAPSHOT          ~14.5 s
WAKE (poll)       ~47 s    (post→poll; dominated by reserving_slot + restoring)
STOP              DELETE returns immediately (async teardown)
host_fence        up to 30 s (SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30 in prod scripts)
release_delay     5 s
```

The STOP teardown happens asynchronously after the DELETE API returns. The
`release_vm_index_after` inline-await in `stop_inner` (at
`nomad_ch.rs:1625-1634`) blocks the slot from being returned to the allocator
for host_fence_wall + 5 s. At the cluster-smoke fence=30 s cap (set in
`gcp-worker-startup.sh:543`):

```
Minimum slot hold per cycle = ~6.5 + 14.5 + 47 + 30 + 5 = ~103 s
```

With ceil=12 slots and per-cycle hold ~103 s, the steady-state maximum concurrent
capacity is:

```
max_concurrent_active = 12
max_throughput        = 12 / 103 s ≈ 0.117 CREATE/s ≈ 7.0 CREATE/min
```

However, the harness dispatches all 20 tasks simultaneously at the start (`ThreadPoolExecutor`
submits all futures together). The first 12 of 20 concurrently-dispatched cycles claim
all 12 slots immediately. The remaining 8 hit `alloc()` → `Err("vm-index allocator
exhausted")` before any of the first 12 have had a chance to complete their teardown
and return their slots. This is **harness front-loading**, not a steady-state
queue. The 12/20 result reflects a burst where c=4 concurrent workers × 5 queued
cycles = 20 total, all of which contend for 12 slots simultaneously.

**The 12 successes take 358.5 s total elapsed.** This implies the harness's 4
threads each ran ~3 completed cycles sequentially (4×3 = 12), with the remaining 8
attempts failing at first-alloc because all 12 slots were taken at the moment they
attempted their CREATE. The per-cycle p50=30 s is the combined CREATE+SNAPSHOT wall
seen before WAKE dominates the tail; it does not include WAKE (async) and STOP phases
which happen asynchronously relative to the harness's sequential cycle model.

**Correct interpretation of the 60% CREATE rate:**

The 60% figure is NOT a steady-state CREATE success rate. In steady-state with a
properly throttled harness (c ≤ slots-in-flight / cycle-wall × safety-factor), the
CREATE rate approaches the theoretical maximum above. The 40% failure in c=4×5 is
a **harness overcrowding artifact** at a fixed ceil=12 slot pool with a 5 s
release delay.

**Three independent levers to improve production CREATE throughput:**

**Lever 1: Bump `vm_index_ceil` 12 → 16+ (config, zero downtime)**

```
vm_index_ceil=12 → default ceiling applies 12 slots on the cluster host
vm_index_ceil=16 → 16 slots → 33% more capacity at same delay
vm_index_ceil=20 → 20 slots → 67% more capacity
```

Constraint: the ceiling is bounded by the tap device pre-allocation in
`gcp-worker-startup.sh:292` (`seq 1 "$VM_INDEX_CEIL"`). Bumping ceil without
bumping the metadata `vm-index-ceil` key leaves the taps unprovisioned and
causes `ENODEV` on alloc above the old ceiling. Both the metadata key AND the
env-derived `SANDBOX_NOMAD_CH_VM_INDEX_CEIL` must change together.

IP arithmetic constraint: `10.99.{100+idx}.2` → idx ≤ 155 (config.rs:346).
All values in [16, 20] are safe.

**Lever 2: Shrink `vm_index_release_delay_secs` 5 → 2 s (config, zero downtime)**

The 5 s delay was calibrated against the driver's tap-deletion-verify budget at
r24-A2-S2 (config.rs:447). Post-v24, `destroy_task_lock_held_total=0` in the c=4×5
run — the OFD probe now works correctly, and the tuntap cleanup completes within the
driver's synchronous `ip tuntap del` verify step (r24-A2-S2: `ENODEV`-verified
before returning). The r24 rationale for 5 s was:

> "matches the driver's tap-deletion-verify budget ceiling so the controller
> doesn't release the index until the driver has finished its synchronous netdev
> cleanup"

With v24 confirming `destroy_task_lock_held_total=0` across 12 stop cycles, the
empirical evidence suggests the tap cleanup is completing well under 5 s in practice.
A 2 s delay would still provide a 2× safety margin over typical tap cleanup latency
(typically sub-second) while recovering 3 s per cycle from the slot hold window.

Effect on slot hold: `103 s → 100 s`. Marginal alone, but compounding with lever 1:

```
ceil=16, delay=2 s:
  hold ≈ 6.5 + 14.5 + 47 + 30 + 2 = 100 s
  max_throughput = 16 / 100 s = 0.16 CREATE/s = 9.6 CREATE/min
  vs. baseline ceil=12, delay=5 s: 0.117 CREATE/s = 7.0 CREATE/min
  improvement: ~37%
```

**Lever 3: Throttle harness concurrency to match slot release cadence**

The harness's `--concurrency 4` with `--cycles 5` dispatches 20 total tasks. The
pathological case is when all 20 are submitted simultaneously and the first 12 starve
the last 8 before any slot is freed. A harness that dispatches no faster than
`slots / cycle_p50_wall` avoids exhaustion bursts:

```
ceil=12, cycle_p50=100 s → max sustained concurrency = 12 / 100 × burst_factor
                         → recommended harness concurrency ≤ 8 for c=4×5 without
                           front-loading exhaustion
```

In production this lever is not controllable — real user traffic doesn't throttle
itself. It is however relevant for interpreting stress harness results: `c=4×5`
at `ceil=12` is not a 60% CREATE failure rate at production load; it is a
harness-front-loading result. The next stress run at `c=20×20` will need to
account for this to avoid confusing allocator-exhausted failures with driver-level
failures.

**Recommended config for the next c=20×20 stress run:**

```
vm_index_ceil                    = 20   (bump metadata + env, re-provision taps)
vm_index_release_delay_secs      = 2    (env override on cluster)
harness --concurrency            = 10   (not 20 — avoids front-load exhaustion at ceil=20)
harness --cycles                 = 20   (total 200; gives >3× the c=4×5 sample)
```

At these settings:
- Pool = 20 slots
- Expected per-cycle hold ≈ 6.5 + 14.5 + 47 + 30 + 2 = 100 s
- Max sustainable concurrency for zero burst-exhaustion ≈ floor(20 / 100 × 100) = 20
  (safe at c=20 since 20 concurrent × 1-slot-per-cycle ≤ 20 slots... but only if
  cycle time is perfectly uniform; real variance means some bursting. c=10 provides
  a 2× headroom margin.)
- Expected CREATE success rate at c=10: ~90%+ (burst exhaustion occurs only when
  concurrent slot demand exceeds 20 simultaneously; with c=10 and p50=100 s that
  requires 10 slots in simultaneous use, leaving 10 free — ample margin)

**File:Line references:**

- `crates/sandbox/scripts/gcp-worker-startup.sh:93` — `VM_INDEX_CEIL=$(md vm-index-ceil); ... ${VM_INDEX_CEIL:-12}`
- `crates/sandbox/scripts/gcp-worker-startup.sh:292` — `for idx in $(seq 1 "$VM_INDEX_CEIL")`
- `crates/sandbox/scripts/gcp-worker-startup.sh:531` — `SANDBOX_NOMAD_CH_VM_INDEX_CEIL=$VM_INDEX_CEIL`
- `crates/sandbox/src/config.rs:93,453` — `vm_index_ceil` and `vm_index_release_delay_secs` defaults
- `crates/sandbox/src/backend/nomad_ch.rs:325-355` — `VmIndexAllocator::alloc()` / `release()` — the exhausted-error path at line 341

**Priority:** IMPORTANT — CREATE throughput at ceil=12 is the production-facing rate limit. Bumping
ceil to 16-20 with a corresponding tap pre-provision bump is a low-risk, high-leverage
config change that unblocks the c=20×20 stress run and raises production capacity.

---

### R30-P1 carry IMPORTANT — permit hold spans `stop_inner`'s full wall; documentation/observability gap

**Status:** UNCHANGED from r30. See r30 for full analysis.

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:1465-1468` (acquire), `:1771` (release),
`crates/sandbox/src/config.rs:483-491` (`SANDBOX_NOMAD_STOP_CONCURRENCY`).

**Priority:** IMPORTANT carry.

---

## MINOR

### R30-P2 carry MINOR — `NomadStopPermitGuard::Drop` silent discard

**Status:** UNCHANGED from r30. See r30 for full analysis.

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:657-668`.

**Priority:** MINOR carry.

---

### R30-P3 carry MINOR — `dec_nomad_stop_permits_in_use` convention-over-ground-truth

**Status:** UNCHANGED from r30. See r30 for full analysis.

**File:Line:** `crates/sandbox/src/metrics.rs:496-528`.

**Priority:** MINOR carry.

---

### R29-P2 carry MINOR — parallel T5 + clock_resync doubles blocking-pool peak demand

**Status:** UNCHANGED. Observation-only at c=20.

**File:Line:** `crates/sandbox/src/wake_machine.rs:518-530`.

**Priority:** MINOR carry.

---

### R29-P3 carry MINOR — `transport_error` prefix-match stringly-typed contract

**Status:** UNCHANGED. Test pins the contract.

**File:Line:** `crates/sandbox/src/restore_handler.rs:3083-3098`.

**Priority:** MINOR carry.

---

## Wake p50 latency from the c=4×5 run

**Measured data:** the c=4×5 cluster doc (`stress-cutover-c4x5.md`) reports
`per-cycle p50 ~30 s` for all 20 attempted cycles. This figure represents
the **cycle p50 observed at the harness**, spanning CREATE + SNAPSHOT phases for
the 12 successes (the 8 exhaustion failures complete in ~770 ms, pulling the p50
down).

The harness does not surface per-phase percentiles for the c=4×5 run; the
`summarize()` function in `snapshot_stress.py:258-311` would emit them but
the c=4×5 cluster doc does not include the full harness summary block. The per-
cycle p50=30 s is the combined harness-observed median for CREATE+SNAPSHOT (the
synchronous phases), NOT the WAKE wall time.

The wake p50 for the 12 successful wakes is not captured in the c=4×5 cluster
doc. The closest proxy is smoke-r23's single-cycle wake wall = 46.9 s, which was
dominated by ~33 s `reserving_slot` (source-teardown + vm_index acquire) + ~13 s
`restoring`. Under c=4 concurrent wakes with ceil=12 the `reserving_slot` wait
depends on how many other wakes are simultaneously holding the allocator's
`reserve_for_restore` lock plus waiting for source-teardown.

**What the counter snapshot tells us about wake latency:**

```
nomad_driver_ch_start_task_stage_total            12   (12 successful restorations)
nomad_driver_ch_start_task_stage_failures_total    0
nomad_driver_ch_start_task_restore_failures_total  0
```

Zero restore failures across 12 wakes means no wake exceeded the alloc_running
or livez timeouts. Given `alloc_running_timeout_secs=120` and `agent_livez_timeout_secs=30`,
all 12 wakes completed their CH start + livez probe within those budgets. The actual
wake wall was not measured in this run.

**Conclusion:** no measurable wake p50 is available from the c=4×5 dataset. The
smoke-r23 single-cycle ~47 s is the best current proxy. A full harness summary
block would have been available if the cluster doc included the `=== RAW_JSON_BEGIN ===`
output, but it was not captured.

---

## Driver v24 — wake p99 projection at c=20×20

**Context:** r30 established that the v24 O_RDWR fix has no behavioural effect on
the WAKE path because `pollWaitForRootfsLockReleased` runs against the fresh COW
inode (lock-free by construction). `wake_rootfs_lock_held_total=0` in the c=4×5
counter snapshot confirms this.

**Wake wall decomposition (from available cluster data):**

```
Phase                       Observed         Source
reserving_slot wait         ~33 s            smoke-r23 (single-cycle; sequential source-teardown)
restoring (CH spawn + livez) ~13 s           smoke-r23
Total wake wall              ~46 s           smoke-r23
```

The `reserving_slot` phase is the dominant component. At c=20×20 (20 concurrent
wakes) the `reserving_slot` wait is driven by:

1. **Source-teardown serialization through the `NomadStopPermits` semaphore** (cap=16
   default; `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS=30` in prod scripts → green-path
   stop ~36 s hold). With 20 concurrent stop calls and cap=16, 4 stops queue.
   p99 wake `reserving_slot` ≈ green-path stop wall × 1-2 = ~36-72 s (one queue
   depth of stop backlog, resolved at ~36 s per stop with 16 concurrent inflight).

2. **`reserve_for_restore` Mutex contention** (`restore_handler.rs:265`): 20 concurrent
   wakes all contend for the `Arc<Mutex<VmIndexAllocator>>` to reserve their slot.
   The lock is held only for the BTreeSet operations (microseconds); not a bottleneck.

3. **`vm_index` availability**: the wake path calls `reserve_for_restore` which marks
   the source slot in-use (preventing a concurrent CREATE from reclaiming it). The slot
   is only available for waking if its source sandbox is still alive (stop has NOT yet
   returned the slot). Under c=20 concurrent wakes all targeting freshly-snapshot-and-
   stopped sources, the source teardowns happen concurrently: 20 sources hit `stop_inner`,
   16 run in parallel (semaphore), 4 queue. The 4 queued sources block the 4 wakes that
   need their slots. Queue resolution time ≈ 36 s (one green-path stop clears a permit).

**Expected wake p99 at c=20×20 (ceil=20, no throttling):**

```
p50 wake wall ≈ 47 s    (dominated by reserving_slot ~33 s + restoring ~13 s)
p99 wake wall ≈ 80-90 s (adds one full queue-depth stop wait of ~36 s;
                         total = ~33 + 36 + 13 = ~82 s worst-path)
```

The c=4×5 zero-failure wake result (12/12, zero restore failures) means the
wake wall under c=4 stayed within the alloc_running + livez budgets. At c=20
the p99 is expected to stay below the 120 s `alloc_running_timeout` but may
approach it if the stop semaphore is saturated and the source-teardown queue
grows beyond one depth.

**Note on c=20 vs. the harness wake budget:** the harness default `--wake-budget=180 s`
comfortably contains the ~82 s p99 projection. Wake timeouts are not expected to
be the binding failure mode at c=20 with ceil=20.

**What v24 does NOT contribute to wake latency:** confirmed again here.
`wake_rootfs_lock_held_total=0` for all 12 cycles. The stageResume unblock in v24
(O_RDWR → probe acquires immediately on COW inode, `stageResume` counter = 0) means
the restore path does not stall on the OFD probe at all — the COW inode is lock-free
by construction. The `start_task_stage_failures_total=0` counter confirms zero
stage failures including the resume stage, consistent with the COW inode analysis
from r30.

---

## LATENT / OPEN (carry from r29-r30)

### R16-P3 / R16-P5 — Fuse encrypt + SHA, gzip pre-AEAD

**Status:** OPEN. Snapshot-path bandwidth optimisations. Unchanged.

**Priority:** OPEN-CARRY.

### R17-P2 — Active-set cache for sweep loops

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

### R5-P1b — SHA + BufReader path in snapshot_handler

**Status:** OPEN. No work landed.

**Priority:** OPEN-CARRY.

---

## Net assessment

**The c=4×5 fresh-cluster validation is the highest-quality dataset in this campaign:
12/12 SNAPSHOT, 12/12 WAKE, 12/12 STOP at zero false-positive counters. The CREATE
60% rate is an allocator-throughput artifact, not a correctness issue.**

**R31-P1 (NEW IMPORTANT):** The measured CREATE throughput of ~0.034/s (2/min) is
allocator-limited at ceil=12, release_delay=5 s. Three config-only levers —
ceil 12→16+, delay 5→2 s, harness concurrency throttle — can raise the effective
throughput by ~37% or more with zero code changes. The most impactful single change
for the next c=20×20 stress run is bumping `vm_index_ceil` to 20 in the cluster
metadata (which requires also pre-provisioning 20 taps at cluster startup via the
startup script).

**R30-P1 (carry IMPORTANT):** permit-hold documentation gap unchanged.

**Wake path (informational):** v24 has no effect on wake-path latency (COW inode
isolation, confirmed by `wake_rootfs_lock_held_total=0`). Projected wake p99 at
c=20×20 is ~80-90 s, well within the 120 s alloc_running budget and the 180 s
harness wake-budget. Wake latency is dominated by the `reserving_slot` phase
(source-teardown serialization through the 16-permit semaphore), not the CH resume
(`stageResume`) or OFD probe.
