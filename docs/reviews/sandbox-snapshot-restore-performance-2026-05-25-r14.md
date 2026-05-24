# Sandbox/snapshot-restore — performance r14 review

Date: 2026-05-25 (UTC)
HEAD at audit: `1482725d` (pilot artifact commit; parents include
`b2892368` C-4 retry, `c890c015` C-3 std::thread::spawn fix,
`c5b9cb9d` R13-Q1 env-mutex unify, `d7740b03` C-5 worker scope).
Round 14 of N. Read-only.

Static review. Cluster-r6 (`T-8b-smoke-retry-r6` @ controller v21)
**confirmed C-4 fixed** and **delivered a fresh SNAPSHOT p50 baseline
6356 ms** (5th cluster cycle to land SNAPSHOT through the Go driver
v4; first to land it post-C-3 with no panic). WAKE failed for a NEW
reason (C-6 — handler silent stall past the vm_index reserve, row
wedged at `restoring`). Cluster has been halted per the sprint
trigger ("5 sequential new bugs across 6 smoke cycles"). No cluster
wake-path latency data exists yet; the budget below is still
code-derived.

## Summary

**3 new perf findings** this round, all C-4-driven, all worst-case
tail-latency in nature:

- **R14-P1 (NEW, INFO/perf-tail)** — C-4 retry-tail latency, 0–118 s
  budget on the wake path. Best-case 0 ms cost (slot free); worst
  observed contention 90 s in cluster-r6 (host_fence ~60 s + Nomad
  purge ~30 s). Per-attempt cost is ~µs. Working through the budget
  table calculus in § "R14-P1 — C-4 retry tail" below.
- **R14-P2 (NEW, MINOR/code-quality)** — `snap-l2-upload-<id-tail>`
  thread name builds the trailing-8-char suffix via a 4-pass char
  walk: `s.chars().rev().take(8).collect::<String>().chars().rev().
  collect::<String>()`. Per-snapshot cost = sandbox-id-length × 2 +
  16 char iterations + 2 fresh String allocations. Negligible
  (~hundreds of ns); flagging only for code-quality posterity.
- **R14-P3 (NEW, INFO/threading)** — C-3 fix replaces a (broken)
  `compio::runtime::spawn_blocking` with a fresh `std::thread::
  Builder::spawn` per snapshot. Threading-cost calculus + bounded /
  unbounded analysis worked through in § "R14-P3 — std::thread vs
  spawn_blocking" below.

No new CRITICAL findings. R10-P1 / R10-P3 / R10-P4 / R10-P5 / R10-P6 /
R10-P7 / R9-#6 / R9-#8 / R11-P1 / R11-P4 carry-forwards unchanged.

Key shifts since r13:

- **C-3 CLOSED (`c890c015`)** — `TieredSnapshotStore::put` now
  detaches the L2 upload via `std::thread::Builder::spawn` rather
  than `compio::runtime::spawn_blocking`. Cluster-r5 + cluster-r6
  confirm no panic. Snapshot p50 effectively unchanged (cluster-r6:
  6356 ms; the L1.put hot path is the only thing on the
  synchronous return).
- **C-4 CLOSED (`b2892368`)** — `reserve_vm_index_with_retry`
  polls the allocator on a `VmIndexRetryPolicy` cadence (default
  60 × 2 s = 118 s effective max sleep budget). Cluster-r6 confirms
  no immediate 503 — but the slot frees and a downstream silent
  stall (C-6) prevents end-to-end measurement.
- **C-5 + R13-Q1 perf-irrelevant** (script-side and test-only,
  respectively).
- **R12-P1 still CLOSED** (BufReader on download_to_disk; lifted at
  `94a8a043`).

## R14-P1 — C-4 retry tail (NEW, INFO/perf-tail)

`reserve_vm_index_with_retry` (`crates/sandbox/src/restore_handler.
rs:265-306`) wraps `RestoreBackend::reserve_vm_index` in a bounded
fixed-interval retry loop. Default policy (`restore_handler.rs:141-
145`): `max_attempts = 60`, `interval = 2 s`. The loop body sleeps
between attempts only when `attempt < attempts`, so effective max
sleep budget = `(60 - 1) × 2 s = 118 s` (`restore_handler.rs:299-300`
matches the `saturating_sub(1)` in the exhaustion log).

Per-attempt cost (success path, slot free):

| Op | Cost |
|---|---|
| `backend.vm_index_retry_policy()` read | sub-µs (returns a Copy struct from a fn) |
| `attempts.max(1)` + loop init | sub-µs |
| First `reserve_vm_index` call | µs-scale (single mutex `.reserve()` against the in-process `VmIndexAllocator` per `nomad_ch.rs:1362`) |
| `attempt > 1` log branch | sub-µs (not taken) |
| Return Ok | sub-µs |
| **Total fast path** | **<10 µs / wake** |

Cluster-r6 confirmed the test wake hit the slow path (sleep loop):
the source teardown released vm_index 1 at `02:26:16` (host_fence
~60 s + Nomad purge ~30 s = 90.2 s wall), and the retry loop was
sized exactly to envelope that. Cost per slow-path wake:

| Phase | Cost |
|---|---|
| 1st attempt fails | µs-scale (mutex .reserve() returns Err) |
| `compio::time::sleep(2 s).await` | ~2 s (compio timer wheel; no thread parked, runtime serves other work) |
| Repeat until 46th attempt succeeds (90 s / 2 s) | ~45 × 2 s = ~90 s, plus 45 × µs failed reserves = noise |
| Success log emit | µs-scale (`info!` macro, structured fields) |
| **Total slow path at observed teardown profile** | **~90 s wall, 0 ntex worker time** |

**Crucial property**: `compio::time::sleep` does not park a worker;
this is structurally a yielding wait. So while the wake-path p99
tail extends by up to the full retry budget, **wake throughput
(jobs-per-second on the controller) is not measurably reduced** —
the only resources held during the retry sleep are (a) one Future
on the executor, (b) a CAS-to-restoring row in pg (gen 3), and (c)
no pg conn (the row was read with a transient pool that's already
been dropped before line 517).

**Wake-path p99 latency budget shift**: best-case 0 ms (cluster-r6
SLA-relevant case: snapshot+wake separated by >90 s — no retry
fires); worst-case +118 s tail (slot never freed in budget,
returns 503). The c=1 cluster-r6 case (snapshot+wake separated by
~76 ms) gave the worst-case retry hit and the slot was released at
~90 s. Updated wake-path budget table in § "Wake-path latency
budget (updated for r14)".

**At c=20 stress** (the carry-forward concern): N concurrent wakes,
each holding distinct vm_index slots, all racing their own source-
teardowns. The retry tail compounds with the R11-P1 pool-churn
correctness-class cliff (the wake row still wedges in `restoring`
during the retry, occupying its generation slot; subsequent /admin
queries against the same sandbox return 409 state-mismatch). At
c=20 the C-4 retry's blocking effect on the row is not perf —
it is a row-wedge tail. **Estimated c=20 wake p99 with C-4 retry**:
the retry budget applies *per* wake, not in aggregate; if all 20
wakes race their own ~90 s teardowns, all 20 wake p99 latencies
land at ~90–120 s. Throughput unchanged. p50 at c=20 unchanged if
slots are free at wake-time (vast majority of production usage —
cold cache, no recent snapshot to race the teardown).

**Calibration confidence**: this is a single cluster sample at c=1.
The 90 s figure is the observed worst case in T-8b-r6. Default
budget (118 s) gives ~28 s margin over the observed worst — fine
for the c=1 sample; under c=20 stress the host_fence p99 may
extend (kernel net-namespace teardown is the bottleneck in
host_fence per nomad_ch.rs comments). **No cluster c=N stress
data exists yet.**

## R14-P2 — `snap-l2-upload-<id-tail>` thread-name builder (NEW, MINOR/code-quality)

`crates/sandbox/src/snapshot_store_gcs.rs:1125-1131`:

```rust
let builder = std::thread::Builder::new().name(format!(
    "snap-l2-upload-{}",
    // Keep the thread name within Linux's 15-char cap by
    // taking the trailing 8 base62 chars of the sandbox id
    // (the entropy bits, not the prefix).
    sandbox_id.chars().rev().take(8).collect::<String>().chars().rev().collect::<String>()
));
```

The "trailing-8-chars" extraction does:
1. `sandbox_id.chars()` — yields an iterator over the full string
   (sandbox_id is a typed-id like `sbx_033M1d8sNTwlK7rxS5syZe`, ~26
   chars).
2. `.rev()` — reverse the iterator (UTF-8 backwards walk; for ASCII-
   only typed-id this is essentially byte-reverse).
3. `.take(8)` — take 8 reversed chars.
4. `.collect::<String>()` — alloc a fresh String (8 chars).
5. `.chars().rev().collect::<String>()` — reverse the String again
   (alloc another fresh String).

Net: 2 fresh String allocations and a ~32-char walk per snapshot.
**Cost per snapshot: probably ~500 ns**. Negligible.

**One-line fix** (only worth doing if anybody touches the line):

```rust
let tail: String = sandbox_id
    .as_bytes()
    .iter()
    .rev()
    .take(8)
    .rev()
    .map(|&b| b as char)
    .collect();
```

(or simpler: `&sandbox_id[sandbox_id.len().saturating_sub(8)..]` — a
byte-slice into the existing String, no alloc, valid because typed-
ids are ASCII). The byte-slice form saves both allocs and the
double-reverse; ~50 ns/snapshot. Below the perf-flagging threshold
but worth the code-quality note since the comment explicitly
documents the convoluted shape.

**R14-P2 status: OPEN.** Sub-µs lever; informational only.

## R14-P3 — `std::thread::Builder::spawn` vs `compio::runtime::spawn_blocking` (NEW, INFO/threading)

The C-3 fix (`c890c015`) replaces the broken-from-spawn-blocking-
worker `compio::runtime::spawn_blocking` with `std::thread::Builder::
spawn` for the L2 detached upload. Brief asks: does the new pattern
shift throughput per worker thread under c=20 stress?

**Bounded vs unbounded analysis**:

| Mechanism | Pool sizing | Backpressure under N concurrent calls |
|---|---|---|
| `compio::runtime::spawn_blocking` (pre-fix, broken) | bounded — compio uses a fixed-size blocking pool (per-runtime config, default num_cpus). | If pool saturated, the call queues; latency rises but no thread explosion. |
| `std::thread::Builder::spawn` (post-fix) | **unbounded** — every call spawns a fresh OS thread. | At c=N concurrent SNAPSHOTs all hitting `Tiered::put`, **N OS threads spawned simultaneously**. Each thread immediately starts the L2 upload via ureq (which uses native TLS + sync I/O). |

**Threading cost per snapshot** (post-fix):
- Thread spawn (clone() syscall + stack allocation) — ~5–20 µs (per
  modern Linux + glibc on x86_64). Below noise.
- Stack reservation — default 8 MiB virtual (committed lazily, so
  actual RSS bump per thread is ~10–100 KiB until the L2 upload
  populates buffers). **Per-thread VSZ +8 MiB; per-thread RSS at
  steady-state +~few MiB during ureq+TLS upload**.
- Thread tear-down — ~5–20 µs.

**c=20 stress scenario**:
- 20 concurrent SNAPSHOTs (one per worker, per minute, or burst).
- Each SNAPSHOT's `Tiered::put` returns Ok within the L1 put time
  (~1–2 s per r13's L1.put estimate) and spawns one L2 upload
  thread.
- At any moment, up to 20 detached L2 threads alive, each holding:
  - 1 ureq Agent + 1 TCP+TLS conn to GCS (~few KB per conn buffer)
  - 1 file handle to the staged L1 artifact (memory-mapped or read-
    streamed; ~few KB per fd)
  - 1 OS thread stack (~8 MiB virtual / ~few MiB resident under
    load).
- Upload wall-time per thread: ~10–60 s on cold GCS pipe (per r13
  estimate); a sustained c=20 snapshot rate produces a moving
  window of ~20 live L2 threads while a new snapshot fires every
  ~3 s.

**Threading cost upper bound under c=20**: ~160 MiB virtual / ~80
MiB resident at any moment (20 × 8 MiB virtual / ~4 MiB resident).
Negligible on n2-standard-32 (128 GiB host). **No throughput
contention** — each thread is doing pure ureq+std::io (no
contention with the compio runtime; the L2 upload is structurally
detached).

**However, sustained c=N at N>>20** would matter:
- Linux default `vm.max_map_count` (65530) limits the total memory
  mappings — at ~3 mappings/thread (stack guard + thread-local-
  storage + mmap), ~20k threads is the practical ceiling. Not
  reachable from a sane snapshot workload, but worth pinning as the
  failure mode in case operators push.
- ureq HTTP/2 multiplexes connections within an `Agent`, but the
  current code constructs a fresh `Agent` per `GcsSnapshotStore`
  call (per R10-P6, still OPEN — `restore_handler.rs:1465-1490`).
  So each detached thread opens its own TLS conn to GCS, no pool
  reuse across L2 uploads. Network pool, not OS thread pool, is
  the c=N ceiling.

**vs the broken pre-fix `compio::runtime::spawn_blocking`**: the
pre-fix code panicked deterministically before any cost was paid,
so this is not a regression — `compio::runtime::spawn_blocking`
never *could* have been the mechanism here, because the call is
issued from a spawn_blocking worker thread that has no compio
runtime in TLS. The C-3 fix doc is explicit (`snapshot_store_gcs.
rs:1084-1108`): pattern A — pull the compio-bound primitive out of
the closure entirely, since the inner work is purely sync std::io
+ ureq.

**Alternative considered**: pass an `Arc<compio::runtime::Runtime
Handle>` into the TieredSnapshotStore so the L2 detach goes through
the compio blocking pool. This would bound the L2 upload
concurrency to the compio pool size (default num_cpus = 16–32 on
n2-standard-32). For the sustained-c=N case it would offer
backpressure, but it requires plumbing the runtime handle through
the snapshot_store constructor + every caller — a wider edit than
the C-3 spike justifies. Worth tracking as a follow-up if c=N
stress shows the L2 upload thread count is a problem in practice.

**R14-P3 status: INFO.** No-op recommended for v1. If c=N stress
later flags OS thread count as a bottleneck (it won't on the
~20 threshold), revisit by routing L2 detach through a bounded
compio blocking pool with the handle passed in at TieredSnapshot
Store construction.

## C-4 wake-path tail latency — projected p99

Brief question: with 60×2s = 118 s retry budget, what's wake p99
NOW?

**c=1, no contention** (slot free at first attempt):
- Retry cost = <10 µs (one mutex .reserve(), no sleep).
- Wake p99 ≈ wake p50; the existing AEAD-disabled budget
  (7.5–10.5 s, calibrated against cluster-r4 CREATE 6494 ms +
  store.get + clock_resync per r13 § "Wake-path latency budget")
  stands unchanged.

**c=1, max contention** (snapshot immediately followed by wake;
source teardown holds slot):
- Cluster-r6 sample: slot held 90.2 s, released at attempt ~46
  (90 s / 2 s = 45 sleeps before success).
- Retry cost = ~90 s.
- Wake p99 = wake p50 (AEAD-disabled) + 90 s = **~97.5–100.5 s**.
- The cluster-r6 client's hardcoded 60 s wake-timeout aborts before
  the retry resolves — but the controller-side wake completes
  (returning 200 on a subsequent /admin/.../wake retry, if the
  client retries). The cluster-r6 C-6 finding is downstream of
  the slot release, not a retry issue.

**c=20 stress** (no data):
- Each wake races its own source-teardown independently.
- host_fence p99 may extend at c=N (kernel veth/netns teardown
  serializes through rtnl_lock); estimated worst case extra ~10 s.
- 20 wake p99s each at ~100–120 s. Retry budget (118 s) is sized
  to envelope; one or two may surface 503 if host_fence p99
  extends >118 s.
- Throughput unchanged (compio time-wheel sleep, no worker park).

**Projection** (code-derived, NOT measured):

| Scenario | Wake p99 (AEAD-disabled) |
|---|---|
| c=1, slot free at first attempt | ~10 s |
| c=1, slot held 90 s (snap-then-wake immediate) | ~100 s |
| c=20, slots free | ~10 s |
| c=20, all racing 90 s teardowns | ~100–120 s |
| c=20, host_fence p99 extends to 130 s | one or two 503s, rest ~120 s |

**Cluster-r6 SLA implication**: if the client retries on a 60 s
wake timeout, the controller-side retry budget covers the worst-
observed teardown wall (90 s) by ~28 s. A client with a 120 s wake
timeout would see no spurious 503s from the C-4 race; with the
existing 60 s wake-timeout, the client times out before the
controller's retry resolves but a retried wake will succeed
(though it falls into the C-6 silent-stall in cluster-r6).

## Cluster-r6 SNAPSHOT 6356 ms — C-3 fix perf delta = ~0

Brief question: did anything in r12/r13 affect snapshot p50? C-3
fix changed L2 detach mechanism — should be ~same.

Confirmed: ~same.

| Cycle | Cluster | SNAPSHOT p50 | C-3 status |
|---|---|---|---|
| `T-8b-smoke-r4` | 1-worker, driver v4 | **PANIC** (spawn_blocking from spawn_blocking) | C-3 OPEN |
| `T-8b-smoke-r5` | 1-worker, driver v4, std::thread fix | **6311 ms** | C-3 CLOSED |
| `T-8b-smoke-r6` | 1-worker, driver v4, std::thread + C-4 retry | **6356 ms** | C-3 CLOSED, C-4 CLOSED |

**Delta r5→r6: +45 ms (0.7%)**. Within single-sample noise on a
single n2-standard-32 worker. The C-3 fix substitutes one detach
mechanism for another, both of which return to the caller
immediately after the spawn handle exists; the synchronous return
path (L1.put) is identical. **No measurable shift in snapshot p50**.

The detached-thread cost on the snapshot return path is the
`Builder::spawn()` syscall plus the thread name format (R14-P2
char-walk = sub-µs). Total: ~5–20 µs per snapshot. Below noise.

**Sustained-c=N L2 thread pile-up**: not perf p50/p99 on the
snapshot RPC itself — the L2 thread is detached. The cost shows up
later as RSS / fd / network conn pressure, surfaced in § "R14-P3
threading cost" above.

## R11-P1 / R13-I1 pool churn at c=20 — re-derived for r14

C-4 retry doesn't change the per-wake DB call count. Pool open calls
per wake (unchanged from r13's calculus):

| open_pool site | Source | Wake-path? |
|---|---|---|
| `get_sandbox_row` | `db.rs:1905` | YES |
| `read_snapshot_row` → `pool_app()` → `open_pool` | `restore_handler.rs:454`, `db.rs:522-524` | YES |
| `update_sandbox_status` (Snapshotted→Restoring CAS) | `restore_handler.rs:364`, `db.rs:1752` (opens pool internally) | YES |
| `update_sandbox_status` (Restoring→Running CAS) | `restore_handler.rs:752` | YES |
| `clear_snapshot_metadata` | `restore_handler.rs:754` | YES |
| **Total per wake** | | **5** |

Per `PoolConfig::default`: `min_idle = 2`, eager-opened in
`connect_with_config`. So **2 pg conns/pool × 5 pools = 10
fresh pg conns per wake**, unchanged from r13's calculus.

C-4 retry sits **between** `read_snapshot_row` (already-completed,
pool dropped) and the next DB call (`update_sandbox_status` to
Restoring at line 364 — happens BEFORE the C-4 retry at line 517).
So the retry holds **zero pg conns** during its sleep; the row's
CAS to Restoring at gen 1 has already been committed.

Re-derived expected pg conn count at c=20 stress (unchanged from
r13):

| c | Inflight pg conns/wake | vs default max_connections=100 |
|---|---|---|
| 1 | 10 | 10% |
| 4 | 40 | 40% |
| 8 | 80 | 80% |
| 10 | 100 | 100% |
| 16 | 160 | **OVER (60%)** |
| 20 | 200 | **OVER (100%)** |

**With C-4 retry layered**: at c=20, 20 wakes each open 10 conns
during the *active* phases. The retry sleep itself holds zero
conns, but the row stays in `restoring` until the slot frees + the
rest of `do_restore_inner` completes. If 20 wakes all race ~90 s
teardowns concurrently:
- t=0: all 20 wakes hit CAS to Restoring → 20 × 5 pools opened
  ahead of the retry (one for read_snapshot_row, one for the
  Restoring CAS). 20 × 2 pools × 2 conns = 80 conns held briefly.
- t=0–90 s: 20 retry sleeps in parallel; **zero conns held**.
- t=~90 s: all 20 retries succeed within seconds of each other →
  20 simultaneous post-reserve open_pools (3 more per wake:
  Running CAS, clear_snapshot_metadata, etc). 20 × 3 × 2 = 120
  conns demanded at once — **already over the 100 default**.

**Implication**: R11-P1 cliff is *deferred* by the retry but not
avoided. C-4 introduces a thundering-herd window — all retry-
satisfied wakes hit the post-reserve DB calls at roughly the
same wall-clock moment (the host_fence release fires for all
slots near-synchronously if all 20 source teardowns started near-
synchronously). **The c≥10 conn cliff is more likely to fire all
at once, not less likely.**

This is a perf-side observation about behavior — not a new finding.
R11-P1 remains the principal fix; the C-4 retry pattern does not
change the conn-exhaustion ceiling, only the timing of the demand
spike.

## Wake-path latency budget (updated for r14, C-4 retry-tail accounted)

C-4 retry adds a tail-latency component bounded by the retry budget.
Budget table refreshed with the tail row:

| Component | Estimate (best–worst) | Source | Calibration |
|---|---|---|---|
| `reserve_vm_index_with_retry` (C-4) | **0–118 s** (best: slot free; worst: full budget exhausted) | `restore_handler.rs:265-306` | cluster-r6 worst-observed = ~90 s |
| `submit_restore_job` (spawn_blocking) | ~2.0–3.5 s | `restore_handler.rs:640-651` | mirrors CREATE submit + alloc |
| `wait_for_livez` (spawn_blocking) | ~1.0–3.0 s | `restore_handler.rs:670-684` | mirrors CREATE livez wait |
| `store.get` AEAD-disabled (L1 hit + SHA verify) | ~0.5–1.5 s | `snapshot_store.rs:225` | bounded by 1 GB SSD read |
| `store.get` AEAD-active (hard-link + decrypt-write) | ~1.0–2.0 s | `snapshot_aead.rs:633-678` (R9-P1) | second 1 GB write to target |
| GCS download (L1 miss; R12-P1 + R11-P2 CLOSED) | ~0.8–2.0 s | `snapshot_store_gcs.rs:438-465` | wall-bound by GCS pipe |
| `clock_resync` (spawn_blocking) | ~0.05–0.15 s | `restore_handler.rs:1582-1668` | unchanged |
| `register_restored` + state.write | <0.01 s | `nomad_ch.rs:1659-1690` | unchanged |
| pg awaits (5 fresh handshakes per R11-P1) | ~0.05–0.2 s | R11-P1 OPEN, corr-class per R13-I1 | bounded by 5× TCP+STARTUP |
| ~32 fresh ureq calls × 1-3 ms TCP 3WHS | ~0.03–0.10 s | R10-P6 OPEN | unchanged |
| Post-store.get diagnostic (3 stat() + format!) | ~0.0001–0.003 s | `restore_handler.rs:570-585` | unchanged |

**Calibrated estimated wake p50 (AEAD-disabled, L1 hit, no C-4 retry):**
~4.4–9.5 s. **Unchanged from r13.**

**Calibrated estimated wake p99 (AEAD-disabled, L1 hit, with C-4
retry at observed 90 s teardown):** ~94.4–99.5 s.

**Calibrated estimated wake p99 (AEAD-active, L1 hit, with full
retry budget exhausted):** ~122–130 s (then returns 503).

**Cluster-r6 caveats**:
- WAKE did not complete in cluster-r6 (C-6 silent stall); the only
  wake stage that ran to completion is up to and including
  `reserve_vm_index_with_retry`. No empirical store.get / submit /
  livez wall times from cluster yet.
- SNAPSHOT p50 6356 ms is the only fresh data point. CREATE p50
  6456 ms (cluster-r6) is consistent with cluster-r5's 6461 ms +
  cluster-r4's 6494 ms (~30 ms noise on a single sample).

**Note on the budget table's "p99" framing**: the C-4 retry-tail
is a tail-latency component, not p50. Adding 118 s to the wake
p50 row is wrong — under no-contention (cluster-r6 single sample
case is the only one that exists, and it was max-contention),
wake p50 ≈ AEAD-disabled budget unchanged. The tail row above
governs p99/p99.9 when N% of wakes race teardowns; in the
cluster-r6 single-sample case, 100% raced and the retry consumed
~90 s.

## C-3 fix — std::thread::spawn perf-path audit

Sibling to R14-P3 — what the new pattern costs *per snapshot* on
the snapshot RPC return path (not on the detached upload).

Pre-fix code at `snapshot_store_gcs.rs:1096` (pre-`c890c015`):
```rust
compio::runtime::spawn_blocking(move || { l2.put(...) }).detach();
```
Would have panicked at `compio-runtime-0.11.0/src/runtime/mod.rs:
119` (no compio runtime in TLS on a spawn_blocking worker thread).

Post-fix code at `snapshot_store_gcs.rs:1125-1161`:
```rust
let builder = std::thread::Builder::new().name(format!("snap-l2-upload-{}", ...));
let spawn_res = builder.spawn(move || { ... });
```

Per-snapshot delta:
- format!() builds the thread name (R14-P2 ~500 ns).
- `Builder::spawn()` invokes clone() syscall (~5–20 µs on Linux).
- Closure capture: `l2: Arc<L2>`, `sandbox_id: String`, `artifact_
  path: PathBuf`, `ch_version_owned: String`, `sha256: [u8; 32]`.
  Move-only — captures are dropped on the spawn-side. Per-capture
  cost: refcount bump on Arc, String/PathBuf moves (no clone).
- Return path: `Builder::spawn() -> Result<JoinHandle>` is dropped
  (handle detached implicitly when the variable falls out of scope
  after the if-let-Err log branch).

**Total return-path cost: ~5–20 µs per snapshot**. Below noise; the
snapshot p50 6356 ms (cluster-r6) is bounded by the L1.put work
(~1–2 s SHA + rename), not the detach.

vs `compio::runtime::spawn_blocking` (if it had worked): the
compio blocking pool reuses worker threads, so the per-call cost
is ~50–500 ns (queue insertion). The C-3 fix is ~10× more
expensive per call but the absolute cost is irrelevant.

**Throughput per worker**: not affected. The snapshot RPC returns
to the client after the spawn; the upload thread runs in
parallel. At c=20 sustained snapshots, ~20 snap-l2-upload threads
live concurrently, each running its own ureq+TLS upload to GCS.
The compio runtime is uninvolved in the L2 upload at all
post-spawn — the OS handles scheduling.

**One concern surfaced by R14-P3**: at very high sustained c=N
(N>>20), the unbounded thread spawn could in principle saturate
the kernel's thread / mmap limits. Not reachable from any
plausible production workload (n2-standard-32 has 128 GiB; the
practical ceiling is the network bandwidth to GCS, which
saturates at <100 concurrent uploads). Documented as a follow-up
if c=N stress shows the L2 detach is a bottleneck.

## Detached-spawn audit — no regressions

Re-scanned `compio::runtime::spawn(\b` and `spawn_blocking\b` in
`crates/sandbox/src/**/*.rs`. Census:

- 14 sites of `compio::runtime::spawn` (registry, sweep, admin_
  handlers, nomad_ch, lib, preview_ws, main) — all pure-async
  bodies; audited individually r10–r13.
- N sites of `compio::runtime::spawn_blocking` — all wrap sync I/O
  (ch.pause / ch.snapshot / store.get / store.put / submit_
  restore_job / wait_for_livez / clock_resync / docker / k8s
  livez). Audited r10–r13.
- **1 NEW site** of `std::thread::Builder::spawn` — C-3 fix at
  `snapshot_store_gcs.rs:1132`. Audited above (R14-P3).

The C-3 fix is the only new threading site since r13. The pattern
is correct (the L2 upload body is pure sync std::io + ureq, no
compio I/O required).

## R12-T `derive_url` 7777 — perf-irrelevant tracker

Brief asks tracking. Searched for the `7777` port literal:

```
crates/sandbox/src/restore_handler.rs:1093  format!("http://127.0.0.1:{}", 7777 + vm_index)
                              :1238  // StubRestoreBackend derive_agent_url
                              :2618, :2702  // test scaffolding
```

All in test code (StubRestoreBackend + integration test stubs).
Real impl (`RealRestoreBackend::derive_agent_url`) lives in
`nomad_ch.rs` and uses `derive_agent_url` from `NomadCHBackend`
with the production subnet shape. No perf impact. Tracker entry
clean.

## Carry-forward (still open from r13 / r12 / r11 / r10 / r9)

| Finding | Status | File:line |
|---|---|---|
| **R11-P1** Every `Database` method opens fresh pg Pool | OPEN (corr-class per R13-I1; thundering-herd window after C-4 retry resolves) | `db.rs:492-516` |
| **R10-P1** AEAD-active snapshot reads memory-ranges 4× | OPEN | `snapshot_aead.rs:377-465`, `snapshot_store.rs:184-223`, `snapshot_store_gcs.rs:540-622, 966-997` |
| **R10-P3** `cipher.encrypt`/`decrypt` allocates fresh Vec/chunk | OPEN | `snapshot_aead.rs:411-446, 529-572` |
| **R10-P4** `GcsSnapshotStore::put` recomputes canonical SHA | OPEN | `snapshot_store_gcs.rs:559-560, 1066` |
| **R10-P5** AEAD encrypt + decrypt write to raw `File` (no `BufWriter`) | OPEN | `snapshot_aead.rs:403, 529` |
| **R10-P6** ~32 fresh `ureq` connections per wake (no pooled `Agent`) | OPEN | `restore_handler.rs:1465-1490` |
| **R10-P7** `clock_resync_random_hex` builds via `format!("{b:02x}")` loop | OPEN | `restore_handler.rs:1687-1772` |
| **R9-P1** AEAD-active wake-path get discards R5-P1 hard-link | OPEN | `snapshot_aead.rs:633-678` (lines 660-674: full plaintext copy to target) |
| **R9-P3** AEAD 3-pass fusable on snapshot put | OPEN (subsumed by R10-P1) | — |
| **R9-#6** 64 KiB scratch buffer inside `ARTIFACT_FILES` loop | OPEN | `snapshot_store.rs:207`, `snapshot_store_gcs.rs:534, 1011` |
| **R9-#8** `chunk_aad` allocates a 13-byte Vec per chunk | OPEN | `snapshot_aead.rs:325-330` |
| **R11-P4** Sweep `SandboxRow::clone` allocation profile | OPEN | `sweep.rs:494-526` |
| **R14-P1 NEW** C-4 retry-tail: up to +118 s on wake p99 | INFO/perf-tail (by design; corr-class won the trade) | `restore_handler.rs:265-306` |
| **R14-P2 NEW** `snap-l2-upload-<id-tail>` thread-name 4-pass walk | MINOR/code-quality (~500 ns) | `snapshot_store_gcs.rs:1125-1131` |
| **R14-P3 NEW** `std::thread::Builder::spawn` for L2 detach is unbounded | INFO/threading (no current bottleneck) | `snapshot_store_gcs.rs:1132` |

## Closed since r13

- **C-3 perf-path** (`c890c015`): `TieredSnapshotStore::put` L2
  detach now uses `std::thread::Builder::spawn`. Verified cluster-
  r5 + cluster-r6 (no panic, snapshot p50 ~6.3 s, no measurable
  shift). Perf-side residual = R14-P2 (code-quality) + R14-P3
  (threading info).
- **C-4 wake-retry** (`b2892368`): `reserve_vm_index_with_retry`
  envelopes the source-teardown release window. Verified cluster-
  r6 (no immediate 503; slot freed at attempt ~46). Perf-side
  residual = R14-P1 (tail-latency budget table updated).

## Ranked next-biggest perf lever (updated for r14)

Unchanged ranking from r13; C-3/C-4 closures do not affect the
levers. Carried forward verbatim:

1. **Hoist `Database` Pool to per-thread `Rc<Pool>`** (R11-P1):
   corr-class per R13-I1; same fix closes the perf nuisance (4× PG
   handshake savings/wake) AND the c≥10 conn-explosion cliff. C-4
   retry doesn't change this; it just times the demand spike. The
   largest single-edit lever.
2. **Eliminate the second 1 GB write on AEAD-active wake** (R9-P1):
   ~0.5–1.5 s/wake saved.
3. **Fix C-6** (next concurrency/correctness sprint): the wake
   handler silent stall past vm_index reserve. Cluster-r6 evidence
   suggests store.get blocking (despite the spawn_blocking wrapper)
   or the post-reserve sync filesystem ops. Out of perf scope but
   required for any cluster wake p50/p99 data.
4. **Fuse encrypt + canonical-SHA + L2-side SHA into the streaming
   pipes** (R10-P1 + R10-P4 + R9-#2): ~1.5–2.5 s/snapshot saved.
5. **`encrypt_in_place_detached` + `decrypt_in_place_detached`**
   (R10-P3): plausible 100–400 ms/AEAD round-trip.
6. **Cached `ureq::Agent`** (R10-P6 / R9-#3): ~30–100 ms/wake.
7. **`BufWriter` on AEAD encrypt/decrypt** (R10-P5): ~100–300 ms
   ceiling per AEAD round-trip.
8. **Sweep allocation cleanup** (R11-P4): sub-ms.

## Notes on focus-area questions

**(C-4 wake-path tail latency)**: best-case 0 ms, worst-case +118 s.
Cluster-r6 c=1 worst case observed: ~90 s retry-tail (host_fence
~60 s + Nomad purge ~30 s). Throughput unchanged (compio time-wheel
sleep, no worker park). p50 unchanged when slots are free at wake-
time. § "R14-P1 — C-4 retry tail" carries the per-scenario table.

**(Cluster-r6 SNAPSHOT 6356 ms vs wrapper)**: C-3 fix delta vs
cluster-r5 = +45 ms (0.7%, within noise). C-3 substitutes detach
mechanism; synchronous return path is identical. § "Cluster-r6
SNAPSHOT 6356 ms" carries the timing chain.

**(R11-P1 / R13-I1 pool churn at c=20)**: 5 open_pool calls/wake ×
2 conns = 10 conns/wake. c=20 → 200 conns demanded → exceeds PG
default 100. C-4 retry sits between two of the 5 DB calls and
holds zero conns during its sleep, but introduces a thundering-
herd window: post-retry, all wakes hit the post-reserve DB calls
near-synchronously. § "R11-P1 / R13-I1 pool churn at c=20 — re-
derived for r14" carries the timing.

**(Wake-path latency budget updated for C-4)**: added the
`reserve_vm_index_with_retry` row at the top: 0–118 s. Other rows
unchanged. § "Wake-path latency budget (updated for r14)" carries
the table. AEAD-disabled wake p99 under cluster-r6's max-
contention case: ~94.4–99.5 s.

**(std::thread::Builder vs compio::spawn_blocking)**: per snapshot
~5–20 µs vs ~50–500 ns. Both below noise on the snapshot RPC
return. Threading-cost concern under c=N stress (each L2 upload
spawns one OS thread): bounded in practice by network bandwidth
to GCS (saturates at ~100 concurrent uploads), not by kernel
limits. § "R14-P3 — std::thread vs spawn_blocking" carries the
calculus.

**(R12-T `derive_url` 7777)**: all 4 sites are test-scaffolding
(StubRestoreBackend + integration test stubs). Production impl is
in `nomad_ch.rs::derive_agent_url`. Perf-irrelevant.
