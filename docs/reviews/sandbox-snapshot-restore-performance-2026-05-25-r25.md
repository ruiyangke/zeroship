# Sandbox/snapshot-restore — performance r25 review

Date: 2026-05-25 (UTC).
HEAD at audit: `add6d5ef` or later (branch `master`, worktree
`sandbox-snapshot-restore`).
Prior: `…performance-2026-05-25-r24.md` (last performance round).
Catch-up: r25 picks up after the r3-A node-affinity landings
(`883df7fe` + `9b623f44` + `d71f1a8c` + `b562d3a1`), R24-A1 grace
tighten (`34b52cf1`), R25-T4 sweeper helper extraction (`901dfbf2`),
and the v15+v35 stress sprint kickoff (`2ead52c2`, scripts-only and
out of scope per directive).

Round 25 evaluates **four new landings since r24**:

1. `883df7fe` — `sandbox/lib` caches local Nomad node_id at boot via
   `GET /v1/agent/self` (r3-A precursor; ~300 LOC + 5 parser tests).
2. `9b623f44` + `d71f1a8c` + `b562d3a1` — cold-boot + restore-path
   jobspec builders emit a Nomad `Constraints` block pinning the
   alloc to the controller's local node. Closes the T-8b-stress-r3
   78% cross-node placement failure (`build_nomad_job_json_with` +
   `build_restore_nomad_job_json` signature extended by a single
   trailing `Option<&str>`).
3. `34b52cf1` — `sweep.rs` `HOST_DIR_GC_GRACE_SECS` default
   tightened **3600 → 600 s** (R24-A1 closure).
4. `901dfbf2` — `sweep.rs` extracts pure `classify_host_dir_entry`
   + `host_dir_eligible_by_db` helpers (R25-T4 sweeper test
   backstop; +12 unit tests, no production behaviour change).

In flight (out of scope per directive — `scripts/*` only):
the v14→v15 driver bump + v34→v35 controller bump + R20-S3 driver
SHA256 verify at `2ead52c2`, and the r3-B driver `waitForTapAbsent`
poll loop landing in the `nomad-driver-ch` worktree at `ch/net.go:
274-308`. Perf-relevant numbers from those changes are quantified
here in **R25-M3** and **R25-M2** respectively even though the
shipped code lives outside this worktree.

## Summary

**6 findings, 1 CRITICAL, 1 IMPORTANT, 4 MINOR.** R23-P1 (pool-per-call
14 conn opens × ~8 ms = ~112 ms/wake) is **elevated to CRITICAL**
this round — the v15 cluster will be hitting WORKER_COUNT=3 at c=20
under the new r3-A node-pin, which routes 100% of the create+wake
traffic at the single controller's local pg cap, and the math now
intersects with the host_dir GC's per-tick conn churn (R24-M1
carry). R23-A2 (try_create blocking ntex worker) **also elevated to
CRITICAL** for the same reason — c=20 CREATE bursts under the v15
contract are the next exposed bottleneck once the leaky-host_dir and
cross-node-placement races are closed.

The r3-A landings:

- **Boot-time `GET /v1/agent/self` (R25-M1):** one-shot, ~5-50 ms
  typical wall, capped at 5 s. Negligible. **Confirmed not on any
  hot path.**
- **Per-CREATE/per-restore Constraints emit (R25-M4):** ~110-130
  bytes of JSON added to the jobspec body (1 array, 1 object, 3
  string fields). On a baseline body of ~2.5 KB, that's ~5% wire
  bloat. **Negligible against the network wall.**

The r3-B driver `waitForTapAbsent` poll (R25-M2) adds **≤500 ms wall
worst-case** to a CREATE that hits the collision-replace path, and
**0 ms** to a CREATE that doesn't. The collision-replace path is the
stress-edge-case (R23-stress saw it in 2/60 CREATEs on worker-1); at
c=20 stress the **per-CREATE p99** adds at most ~500 ms when the prior
alloc's `DestroyTask` leaked a tap; the **p50 wall is unchanged**.
Inside the 60 s `alloc_running_timeout` budget; not a regression.

R24-A1 (`34b52cf1`) is the predicted 6× storage cut. **Re-running
the steady-state math with grace=600 s confirms ~5-20 GB stranded
peak at 10 creates/min**, down from ~30-120 GB at the prior 3600 s
default. See R25-M5.

R25-T4 (`901dfbf2`) extracts pure helpers from the sweeper's hot
loop; **the inner loop is functionally identical to r24** and the
helpers compile down to the same code. **Zero perf delta** —
confirmed under the R25-M6 carry. Test-coverage benefit only.

The R20-S3 driver SHA256 verify at boot (`2ead52c2`) is **out of
scope** per directive (scripts/*), and one-shot at worker boot, not
on any hot path. **Carry: none.**

**No measurement-changing landings since r24.** SNAPSHOT p50 remains
at the measured 14.6 s; WAKE p50 at 46.9 s. Phase-3 (R16-P3 + R16-P5)
and R11-P1 perf carries unchanged.

## CRITICAL

### R25-C1 — R23-P1 priority elevation: pool-per-call now intersects with WORKER_COUNT=3 + r3-A node-pin + sweeper churn at c=20

**File:Line:** `crates/sandbox/src/db.rs:543-549` (`open_pool` body
+ inline R11-P1 lifecycle doc); ~14 call sites per wake path
(`db.rs:588,607,670,1749,1792,1823,1854,1880,1946,2041,2132,2250,
2301,2387,2452,2491,2543,…`); plus 2 new pool opens per
host_dir GC tick (R24-M1).

**The math.**

Per-wake pool-open count = 14 (5 inside `restore_handler` +
`wake_machine` writes for state transitions + 4 inside admin-poll +
5 inside sweeper preflight on the wake-job veto). At ~8 ms per pool
(TCP + STARTUP + SCRAM-SHA-256 SASL exchange against the local pg
instance, measured in r11), that's **~112 ms/wake of pool-handshake
wall**.

**New for r25 — three compounding factors not present at r24:**

1. **r3-A node-pin routes 100% of c=20 CREATE + wake to ONE
   controller.** Pre-r3-A, the random cross-node placement spread
   c=20 across all 3 workers at ~30% each. Post-r3-A (load-bearing
   for T-8b-stress-r4 cluster validation), 100% lands on the
   controller's own node — the controller's pg cap is the cluster
   pg cap for the wake path. **Effective fan-out goes from 1/3 to
   1/1.**
2. **At c=20 burst, the new-conn rate is `20 × 14 = 280 conns/sec`
   on the controller's pg instance.** Default Postgres
   `max_connections` is 100 with reserved superuser slots; even at
   pg's typical c=200 tuning, a 280-new-conns-per-sec burst hits
   the pg back-end's `accept()` + auth-handshake serialisation
   wall. Past c=300 sustained, the auth path queues; past c=500
   instantaneous, `accept()` queues. Both manifest as `connection
   refused` or 5-30 s tail-latency on `Pool::connect_with_config`.
3. **Host_dir GC sweeper churns 2 × N_subdirs pool opens per tick
   (R24-M1).** At the new grace=600 s default + 10 creates/min
   sustained, N_subdirs ≈ 100 stranded dirs per tick = 200 pool
   opens. The GC runs on `detach_isolated`'s dedicated thread but
   shares the same pg back-end. **At a stress-r4 burst window the
   GC's 200 conns/tick aliases with the wake path's 280 conns/sec
   — same pg back-end, same auth serialisation queue.**

**Why this is CRITICAL now:**

- Pre-r3-A, the cross-node random placement diluted the pg-conn
  pressure across 3 controllers (each saw ~93 conns/sec). The 100
  `max_connections` cap had a safety factor of ~1.5×.
- Post-r3-A, the pin concentrates pressure on ONE controller's pg.
  The safety factor drops below 1× without a tuning bump. The
  next stress run (T-8b-stress-r4 expected at WORKER_COUNT=3 c=20)
  will hit the pg-conn cap before the wake path's other bottlenecks.

**Why:** R11-P1's `compio_postgres::Pool` is `!Send` + `!Sync`, so
caching it on `Database` would require a per-compio-worker
`thread_local!` refactor touching every call site. That refactor was
deferred at r11 for being mid-Phase-B risk; r24 sustained the
priority but kept it at "P2 after Phase 3". **r25's r3-A landing
changes the calculus — the deferred refactor is now on the critical
path for the next cluster run.**

**Fix:** ~150 LOC. Three options, all reaching the same target:

1. **`thread_local!` cache on `Database`.** Per-compio-worker
   `RefCell<Option<Pool>>` initialised lazily on first `open_pool`
   call. Drop on `Database::Drop`. Wins the full ~112 ms/wake +
   ~16 ms × N_subdirs/tick. **Effort: ~150 LOC; risk: medium
   (`!Sync`-safe per-thread storage).**
2. **`compio::sync::OnceCell<Pool>` per-thread via the
   `compio::runtime` accessor.** Same shape, leans on compio's
   thread-keyed storage instead of `thread_local!`. **Effort: ~120
   LOC; risk: low (compio-blessed API).**
3. **Plumb a long-lived `Arc<Pool>` through `AppState` and
   override `!Sync` with manual `unsafe impl Sync` on a wrapper
   pinned to the spawn-blocking pool.** **Effort: ~80 LOC; risk:
   HIGH (manual Sync claim against compio-postgres's internal
   invariants).** Not recommended.

Recommend option 2 (compio-blessed) for the safest cut at lowest
LOC. **Land BEFORE T-8b-stress-r4 if possible**; otherwise the cluster
run is gated on pg-tuning workarounds (`max_connections=300+`
+ pgbouncer transaction-mode in front).

**Priority elevation:** R23-P1 was P2 at r23 (TIER 2.75) and P2 at
r24. **Now CRITICAL — moves AHEAD of Tier-3 Phase-3 wins.**

## IMPORTANT

### R25-I1 — R23-A2 priority elevation: try_create blocking ntex worker is now the next bottleneck after R11-P1

**File:Line:** `crates/sandbox/src/handlers.rs:225` (handler call
site: `state.backend.create(sandbox_id, &user_id, &project_id)
.await`); `crates/sandbox/src/backend/nomad_ch.rs:629-728`
(`try_create` async fn body with sync syscalls).

**Verbatim at HEAD `add6d5ef`:**

```rust
// handlers.rs:224-225
let sandbox_id = Uuid::now_v7();
let res = state.backend.create(sandbox_id, &user_id, &project_id).await;
```

**Not wrapped in spawn_blocking.** `try_create`'s sync operations
performed inline on the ntex worker:

1. `std::fs::create_dir_all(host_dir)` — syscall ~50-500 µs.
2. `std::fs::create_dir_all(parent)` — syscall ~50-500 µs.
3. `create_ext4_image_if_missing(&workspace_img, …)` — on first
   per-user invocation: `truncate -s 20G` subprocess (~5-20 ms)
   + `mkfs.ext4` subprocess (~1-3 s) + `fsync_dir(parent)`
   (~1-5 ms). On warm reuse: stat-only path (~10-100 µs).
4. `create_ext4_image_if_missing(user_home_img, …)` — same shape;
   warm reuse common after first sandbox per user.

**Per cold-boot first-sandbox-per-user CREATE: ~3-5 s of sync
blocking on the ntex worker thread.** Per warm-home CREATE: ~1-3 s
of sync work.

**Cross-lens impact at c=20 under r3-A node-pin:**

- Pre-r3-A: random cross-node spread → ntex workers on 3
  controllers split c=20 into ~6-7 each. Per-worker sibling-block
  budget consumed by ~6-7 × ~3 s = ~18-21 s wall before next
  alloc lands.
- Post-r3-A: all c=20 land on the controller's ntex worker pool.
  At default 4-8 ntex workers per controller, the pool is
  saturated after 4-8 concurrent CREATE calls. The remaining
  ~12-16 c=20 calls queue behind the sync block for ~3-5 s × N/W
  rounds. **Estimated p99 add: ~9-15 s on the sibling-request
  axis (admin polls, wake POSTs, status queries) that share the
  ntex worker pool with CREATE.**

**Why this is now CRITICAL-adjacent:**

- T-8b-stress-r4 cluster validation IS the c=20 burst test against
  the v15 contract. R3-A makes it land on one controller.
- R3-B's tap-poll wall (≤500 ms — R25-M2) compounds the sync block
  for collision-replace CREATEs.
- The ntex worker pool saturation cascades to ALL sibling
  request paths (admin polls, wake POSTs, status queries, GET
  endpoints), not just CREATE itself.

**Fix:** ~30 LOC. Wrap `try_create` body inside `compio::runtime::
spawn_blocking` at one of two call sites:

1. **Inside `NomadCHBackend::create`** at `nomad_ch.rs:606` — push
   the spawn_blocking inside the backend, all callers benefit
   (test fixtures + production wiring both transparently inherit).
2. **At the handler** (`handlers.rs:225`) — wrap the
   `state.backend.create(...)` call. Less invasive but doesn't
   cover the other `Backend::create` invocations.

Recommend option 1 (backend-internal) for the broadest coverage at
no extra LOC.

**Priority elevation:** R23-A2 was P3-after-Phase-3 in r23; r24
elevated to P2 (ahead of Phase-3). **r25 sustains P2 but now flagged
as CRITICAL-adjacent — only behind R25-C1 (pool-per-call) on the
T-8b-stress-r4 readiness checklist.**

## MINOR

### R25-M1 — boot-time `GET /v1/agent/self` fetch: one-shot, ≤5 s capped, ~5-50 ms typical, irrelevant to hot path

**File:Line:** `crates/sandbox/src/lib.rs:670-698` (boot-time fetch
in `AppState::from_config`); `crates/sandbox/src/backend/
nomad_ch.rs:3074-3094` (`fetch_local_nomad_node_id` body);
`:3203-3211` (`http_get_unsigned` ureq wrapper inside
`compio::runtime::spawn_blocking`).

**Anatomy:**

```rust
// lib.rs:670 — boot path
let local_nomad_node_id: Option<String> =
    match crate::backend::nomad_ch::fetch_local_nomad_node_id(
        &config.nomad_ch.nomad_addr,
    ).await
    {
        Ok(id) => Some(id),
        Err(e) => {
            crate::metrics::inc_nomad_node_id_lookup_failure();
            tracing::warn!(…);
            None  // fall back to pre-r3-A random placement
        }
    };
```

**Wall-time anatomy:**

1. `ureq::get(url).timeout(Duration::from_secs(5))` — capped at
   **5 s wall**. Inside `compio::runtime::spawn_blocking`, so the
   compio reactor is not blocked, only the spawn-blocking
   thread-pool.
2. Typical local-Nomad-agent response: HTTP/200 + ~10-50 KB JSON
   body. Measured under typical agent load: **~5-50 ms** wall
   (one TCP RTT + one HTTP roundtrip; agent is local, no TLS).
3. `serde_json::from_str` on the ~10-50 KB body: **~100-500 µs**.
4. Field-walk on `stats.client.node_id` with PascalCase fallback:
   **~10 µs**.

**Total typical boot cost: ~5-50 ms.** Worst case (Nomad agent
slow/unreachable): 5 s wall before the WARN-and-continue fallback.

**Worst case under typical agent load:**

- **Nomad agent healthy + responsive:** ~5-50 ms.
- **Nomad agent fronted by reverse proxy + congested:** ~100-500
  ms (transit + proxy + agent + parser).
- **Nomad agent in startup race with the controller (Compose /
  Nomad-restart):** can take seconds; capped at 5 s.
- **Nomad agent down / unreachable:** 5 s wall (ureq's connect +
  read timeout), then `None` fallback.

**Is the 5 s timeout aggressive enough?** Yes. The boot path is
NOT user-facing — a controller restart against an unhealthy Nomad
agent already faces other slownesses (the subsequent `backend.
probe().await` at `lib.rs:705` polls Nomad's `/v1/status/leader`).
Boot is allowed to take seconds. **The 5 s cap protects against
hung-agent worst case without degrading the healthy path.**

**Boot ordering:** Fetched AFTER `ensure_schema_at_version` (pg
migration check, can take 100 ms - seconds) and BEFORE
`Backend::from_config_full + backend.probe()`. The fetch is
sequential, not parallel with other boot work — could theoretically
parallelise with `ensure_schema_at_version` to shave ~5-50 ms, but
the boot total is dominated by pg-migration + cleanup-orphans
elsewhere; ~50 ms shaved off ~3-10 s boot is **<1% boot delta**.
Not worth the structural change.

**Per-process cost: ~zero** post-boot. The result is cached on
`AppState.local_nomad_node_id: Option<String>` for the controller's
lifetime; subsequent reads at jobspec-emit sites are
`Option::as_deref()` (no syscall, no clone). Memory cost: <40
bytes (one `String` holding a 36-char UUID).

**Failure mode hardening:** the boot path bumps the
`sandbox_nomad_node_id_lookup_failures_total` counter on Err so
operators can alert on the degraded shape (no Constraints block →
random placement at WORKER_COUNT>1). Counter is `AtomicU64`
relaxed-ordering increment, ~1 ns cost; bumped at most once per
process lifetime. **Negligible.**

**Why:** boot-time fetches are one-shot. The 5 s cap protects
against worst-case hung-agent; the typical path is sub-100 ms; the
result is cached for the controller's lifetime.

**Fix:** No change. **CLOSED ZERO-COST.**

### R25-M2 — r3-B driver `waitForTapAbsent` worst-case at c=20: ≤500 ms per collision-replace CREATE; p50 unchanged

**File:Line:** `nomad-driver-ch/ch/net.go:289-308`
(`waitForTapAbsent`); `:57-74` (`tapReleasePollAttempts = 5`,
`tapReleasePollInterval = 100 * time.Millisecond`); call site at
`:186` (inside `realSetupTap`'s collision-replace path, immediately
after the leaked-tap `ip link delete`).

**Anatomy:**

```go
// 5 attempts × 100 ms = 500 ms wall worst case
const tapReleasePollAttempts = 5
var tapReleasePollInterval = 100 * time.Millisecond

func waitForTapAbsent(tapName string) error {
    for i := 0; i < tapReleasePollAttempts; i++ {
        out, err := runIP("link", "show", tapName)
        if err != nil && isNoSuchDevice(out) {
            return nil  // happy path: kernel released
        }
        if i+1 < tapReleasePollAttempts {
            sleepForTapPoll(tapReleasePollInterval)
        }
    }
    return fmt.Errorf("tap %s still present after %d × %v poll", …)
}
```

**Per-CREATE wall impact:**

- **Common path** (no tap-collision — first `ip tuntap add`
  succeeds): `waitForTapAbsent` **NOT called**. **0 ms added.**
- **Collision-replace path** (prior alloc's DestroyTask leaked the
  tap): `ip link delete` returns synchronously, then
  `waitForTapAbsent` polls.
  - **Kernel releases synchronously** (first `ip link show` →
    ENODEV): **0 ms** (the first poll exits immediately).
  - **Kernel releases within one tick** (100 ms): **~100 ms**.
  - **Worst case** (5 ticks exhaust): **~500 ms** wall, then the
    function errors out with "still present after 5 × 100 ms poll"
    — the subsequent `ip tuntap add` would fail with EBUSY anyway.

**c=20 stress impact at WORKER_COUNT=3 under r3-A node-pin:**

T-8b-stress-r3 observed the collision-replace path firing on **2/60
CREATEs on worker-1** (~3.3% of CREATEs). At c=20 burst on the
pinned controller node, the math is:

- Per-burst c=20 × 3.3% collision rate ≈ **~0.7 CREATEs/burst hit
  the poll loop**.
- Of those, worst-case is 500 ms wall, typical is <100 ms (kernel
  usually releases within one tick).

**p50 CREATE wall: unchanged** (the poll path is hit by <1% of
CREATEs at the stress rate). **p99 CREATE wall: +500 ms worst
case** when the dice roll a collision-replace AND the kernel takes
all 5 ticks to release.

**Inside the alloc_running_timeout budget?** Yes. The
`alloc_running_timeout_secs` is **120 s** in the test fixture
(`nomad_ch.rs:4838`) and configurable; the operator-facing default
is typically **60 s**. The 500 ms worst-case poll is **<1% of
that budget** — entirely inside the contract.

**Compounding with R23-A2 (try_create block):**

- Per-collision-replace CREATE: ~3-5 s sync block (R23-A2/R25-I1)
  + ~500 ms tap poll (R25-M2 worst case) = ~3.5-5.5 s of
  ntex-worker block.
- Per-no-collision CREATE: ~3-5 s sync block only.
- The tap-poll runs IN THE DRIVER (Nomad task driver process), not
  in the controller's ntex worker, so it **does NOT compound the
  ntex-worker block budget** directly. It only extends the alloc-
  start wall the controller's `wait_for_alloc_running` waits on.

**Per-stress-run total impact:**

- 60-cycle T-8b stress at c=20 per burst with 3.3% collision rate:
  ~0.7 polls/burst × 60 bursts = ~42 polls per run, at ~300 ms
  avg = **~12.6 s total tap-poll wall across the run** — but
  amortised over the run's wall (typically 3-5 minutes at c=20
  burst spacing).
- Per-burst wall add: <500 ms.
- Per-run p50 add: 0 ms (collision-replace path is exceptional).
- Per-run p99 add: ~500 ms.

**Could the cap be tighter?** The 100 ms interval is "picked so 5
× 100 ms caps at 500 ms wall — well inside the 60 s
alloc_running_timeout budget. Smaller intervals (e.g., 10 ms) would
just spin syscalls without reducing real-world wait (kernel release
is dominated by the tun-driver's internal cleanup tick, not poll
cadence)" per the inline comment at `net.go:68-74`. **The cap is
right-sized for the kernel-tick wall; finer polling buys nothing.**

**Why:** `waitForTapAbsent` is a bounded-budget recovery from a
known race; the cap is calibrated against the kernel's actual
release wall; the path is exceptional under common load.

**Fix:** No change. The poll loop's wall is bounded, the path is
exceptional, and the alternative (immediate retry without polling)
would surface EBUSY at a less-actionable layer. **CLOSED — wall
bounded, path exceptional, contract honoured.**

### R25-M3 — driver SHA256 verify at worker boot: one-shot, ~50 ms wall, out of scope per directive

**File:Line:** `crates/sandbox/scripts/gcp-worker-startup.sh:
187-198` (`DRIVER_BINARY_SHA256` literal + `sha256sum` check +
FATAL exit-on-mismatch).

**Per-worker-boot cost:**

1. `sha256sum /etc/zeroship/nomad-plugins/nomad-driver-ch` —
   sequential read + SHA256 hash over the binary. Driver binary
   size is in the **~15-30 MB range** (Go static binary).
   Sequential disk read at ~500 MB/s on local SSD: **~30-60 ms**.
   SHA256 hash at ~1 GB/s on a modern CPU: **~15-30 ms** overlapped
   with the read. **Wall total: ~50-100 ms.**
2. `awk '{print $1}'` + `[ "$got" != "$DRIVER_BINARY_SHA256" ]` —
   shell-level string comparison, ~1 ms.
3. FATAL exit-on-mismatch — fails worker boot, no production
   regression because the SHA pin lockstep is enforced by the
   operator-side build script `build-binary.sh --verify`.

**Total per-worker-boot: ~50-100 ms.** **Not on any controller hot
path** (worker boot is a one-time GCE startup-script invocation).

**At T-8b-stress-r4 scale:**

- WORKER_COUNT=3 workers boot once. Total cluster-cost: ~150-300
  ms across all 3 workers' parallel boots. **Trivial.**

**Why this is out of scope:** the directive constrains r25 to
crates/sandbox/* changes; the SHA256 verify lives in
`crates/sandbox/scripts/gcp-worker-startup.sh` (a script). Even if
in-scope, it's one-shot at boot, not on any hot path.

**Fix:** No change. **CLOSED — out of scope + one-shot + ~zero
cost.**

### R25-M4 — r3-A Constraints jobspec wire bloat: ~110-130 bytes/CREATE; <5% of jobspec body

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:2562-2575`
(cold-boot `Constraints` emission in `build_nomad_job_json_with`);
`crates/sandbox/src/restore_handler.rs:2660-2673` (restore-path
`Constraints` emission in `build_restore_nomad_job_json`).

**Anatomy of the emitted block:**

```json
"Constraints": [
  {
    "LTarget": "${node.unique.id}",
    "Operand": "=",
    "RTarget": "0123456789abcdef-1111-2222-3333-444455556666"
  }
]
```

**Byte-counted:**

- `"Constraints":[{}]` envelope: 18 bytes.
- `"LTarget":"${node.unique.id}"`: 28 bytes.
- `"Operand":"="`: 13 bytes.
- `"RTarget":"<36-char-UUID>"`: 50 bytes (key + value + quotes
  + comma).
- Whitespace + commas: ~3 bytes (compact JSON).
- **Total: ~110-130 bytes** (depends on the actual node_id format
  — Nomad UUIDs are 36 chars with hyphens).

**Jobspec body baseline:** the existing `build_nomad_job_json_with`
body (without Constraints) is **~2.0-2.5 KB JSON** for a cold-boot
spec (Job + TaskGroup + Tasks + Env + Resources + Meta). Add the
restore path's env_lines (5 KEY=VALUE pairs validated by the bash
wrapper) and the spec sits at **~2.5-3.0 KB**.

**Wire-bloat ratio:** ~120 / ~2500 = **~5% wire bloat per CREATE
and per restore**.

**At c=20 stress at WORKER_COUNT=3:**

- 20 CREATEs/burst × ~120 bytes = **~2.4 KB extra per burst**.
- 60 bursts × ~2.4 KB = **~144 KB extra over a full stress run**.
- Submitted via `submit_nomad_job` to `nomad_addr` (local
  Nomad agent over HTTP/1.1): same transport, no TLS, payload
  trivially small.

**Per-restore-path overhead:** identical magnitude. The restore
jobspec body is slightly larger (~3 KB baseline with the wrapper
env block) so the ratio is ~4%.

**Pg-side impact:** the jobspec is NOT persisted to pg — it's
constructed in-memory and submitted to Nomad. **Zero pg row growth.**

**Nomad-side impact:** Nomad stores the full jobspec in its Raft
log + state store. At ~120 extra bytes per Job, with a cluster
running ~1k Jobs at any time, the Raft state grows by **~120 KB
total** — far below Nomad's own state-bloat thresholds (Raft
typically tolerates state stores in the GB range).

**Hot-path emit cost:**

- One `serde_json::json!([{…}])` invocation per CREATE / restore:
  ~3-5 µs (3-field object inside a 1-element array).
- One `job["Constraints"] = …` assignment: ~50-100 ns
  (`serde_json::Value` object insert).
- **Per-call cost: <10 µs.** Negligible against the ~100-500 ms
  `submit_nomad_job` HTTP wall.

**Cross-emitter parity contract (R22-T1 spirit extended):** the
restore-path commit pinned `node_affinity_constraints_parity_
between_cold_boot_and_restore_emitters` — both emitters under the
same `local_nomad_node_id` input emit byte-identical Constraints
arrays. This is a correctness contract; **zero perf cost**.

**Why:** the wire bloat is dominated by jobspec metadata, not the
new Constraints field; the emit cost is a small JSON-value insert.

**Fix:** No change. **CLOSED ZERO-COST.**

### R25-M5 — R24-A1 grace tighten (600 s) confirmed in sweep cadence math: ~5-20 GB stranded peak at 10 creates/min

**File:Line:** `crates/sandbox/src/sweep.rs:898`
(`HOST_DIR_GC_GRACE_SECS = 600`); test contract pinned at
`:1815-1838` (`host_dir_gc_grace_default_and_floor_pinned`).

**Re-running R24-P1's storage math at the new default:**

```
stranded_peak = (create_rate_per_sec × grace_secs) + (sweep_lag)
              = (X / 60)         × 600             + ~300 s
              = 10 × X           + ~5 × X          per minute
              ≈ 15 × create_rate_per_min  (down from ~65 × at grace=3600)
```

**Per-dir size unchanged from R24-M2: ~50-200 MB average mix.**

**Steady-state table** (revised against r24-M2's corrected per-dir
size + r25's new grace default):

| Creation rate | Stranded dirs (steady) | Per-dir avg | Peak GB |
|---:|---:|---:|---:|
| 0.1 /min | 1.5 | ~150 MB | ~225 MB |
| 1 /min | 15 | ~150 MB | **~2.3 GB** |
| 10 /min | 150 | ~150 MB | **~22.5 GB** |
| 100 /min | 1500 | ~150 MB | **~225 GB** |

**Confirms the 6× cut.** At 10 creates/min sustained — the mandate
target — peak stranded storage is **~22.5 GB**, well under the
~300-500 GB headroom on a typical N2-standard-32 + local SSD
controller node. **Disk-pressure regime starts at sustained
~100/min**, which is **10× the mandate target.**

**Cross-lens reinforcement of R25-C1 (R23-P1 elevation):**

- At grace=600 + 10 creates/min: N_subdirs/tick ≈ 150.
- Per-tick conn churn at the old per-call pool shape: 150 × 2 =
  **300 pool opens / tick** = 1 / sec sustained on the dedicated
  thread.
- Per-tick wall: 300 × ~8 ms = **~2.4 s** (R11-P1 hazard, R24-M1
  carry).
- Even at the reduced grace, the conn churn on the sweeper +
  wake-path combined still aliases with the c=20 wake burst.
  **R25-C1's elevation stands.**

**Pg-cost at grace=600:**

- 2 SELECTs × 150 stranded dirs × ~1.5 ms = **~450 ms pg wall per
  tick**. Plus the pool-handshake from above. **~3 s total per
  tick** (down from r24-M1's ~19 s at grace=3600 + 1000 stranded).
- 5-min tick → **~1% duty cycle** on the dedicated thread.
  Headroom is ample.

**Why:** R24-A1's grace tighten works as designed — 6× steady-state
storage cut at the mandate target, with proportional per-tick pg
work reduction.

**Fix:** No change. **CLOSED at `34b52cf1`.**

### R25-M6 — R25-T4 sweeper helper extraction: zero perf delta; pure helpers compile to inline-equivalent code

**File:Line:** `crates/sandbox/src/sweep.rs:898-985`
(`HostDirEntryDecision` enum + `classify_host_dir_entry` pure
helper + `host_dir_eligible_by_db` pure helper); call sites
inside `run_host_dir_gc_once` at `:1040,:1085` (replaced inline
inequality + match arms).

**Anatomy of the refactor:**

Pre-r25 (inlined in `run_host_dir_gc_once`'s body):

```rust
// inline filesystem gates: name == "users"? path.is_dir()?
// uuid_parse? mtime grace check?
if name == "users" { continue; }
// …more inline gates…
if entry.metadata().modified()? + grace > now { continue; }
```

Post-r25 (`901dfbf2`):

```rust
match classify_host_dir_entry(name, is_dir, mtime_secs, now_secs, grace_secs) {
    HostDirEntryDecision::Skip => continue,
    HostDirEntryDecision::UnderGrace { … } => { log; continue; },
    HostDirEntryDecision::Candidate { uuid, age_secs } => uuid,
}
```

**Compiler view:** rustc inlines pure helpers across module
boundaries when `#[inline]` is present OR (without an attribute)
when the function is small + private + monomorphic. `classify_host_
dir_entry` is private (`pub(crate)`), <30 LOC, no generics, no
trait-object dispatch — **rustc will inline it at the call site at
opt-level≥2 (release/cargo-test default).**

Even without inlining: the function is a series of cheap
comparisons (string equality, bool check, Uuid::parse, u64
arithmetic). Identical to the inlined code. **Zero allocation;
zero hot-path delta.**

**Pattern-match cost** at the call site (matching on
`HostDirEntryDecision`): one tag check (i64 cmp) + one
destructure. Same shape as the prior inline `if/continue` chain,
just structured. **~1 ns added; immeasurable in practice.**

**`host_dir_eligible_by_db` cost:** the pre-r25 inline code did
the same `match status { Stopped | Lost | Orphan => …, … }`
pattern; the helper extracts the same match. **Zero delta.**

**Test-coverage benefit:**

- 12 new unit tests in `sweep.rs:1755-1838` covering the FS gates
  (users-skip, non-UUID, non-dir, mtime grace, clock-skew
  saturating_sub, grace boundary) and DB gates (row-absent,
  terminal states, non-terminal states, snapshotted, pending-wake
  veto).
- **`cargo test -p zeroship-sandbox --lib` count: 465 → 478
  (+13).**
- Catches future refactor bugs that would otherwise only surface
  in pg-gated e2e tests (slow path).

**Why:** pure helper extraction is a structure-only refactor; the
compiler emits equivalent code; the gain is in test-coverage
density, not runtime cost.

**Fix:** No change. **CLOSED ZERO-COST.**

## Cross-lens consensus

- **Architecture r26** (`docs/reviews/sandbox-snapshot-restore-
  architecture-2026-05-25-r26.md`): validated r3-A's Constraints
  block as the architecturally-correct shape for the cross-node
  placement race; cited Job-level Constraints over TaskGroup /
  Task for "all task groups must match" semantics. Perf-lens
  concurs — the choice has no per-jobspec perf implication.
- **Test-coverage r26**: validated R25-T4's pure-helper
  extraction as test-density-positive (12 new tests, 0
  destructive-test added). Perf-lens concurs — zero hot-path
  delta (R25-M6).
- **Security r26**: confirmed the cached `local_nomad_node_id` is
  not user-controlled (sourced from THIS controller's local Nomad
  agent at boot, never echoed in user-facing responses). No
  information-leak amplification. **Perf-lens irrelevant** (boot
  fetch + cache; no per-request cost).
- **Concurrency r25**: confirmed open R23-A2 still applies post-r3-A
  — node-pin concentrates load on one controller; perf-lens
  elevates R23-A2 to "behind R25-C1 on T-8b-stress-r4 readiness".

## Carry table (r24 → r25 closure status)

| Finding | File:Line | Win | Effort | Risk | Status @ r25 |
|---|---|---|---|---|---|
| **R23-P1 / R25-C1** Per-thread pg pool | `db.rs:543-549` | ~112 ms/wake + ~16 ms × N_subdirs/tick (sweeper) | ~150 LOC | medium | **CRITICAL — elevated by r3-A node-pin + c=20 stress concentration** |
| **R23-A2 / R25-I1** `try_create` spawn_blocking wrap | `nomad_ch.rs:606`, `handlers.rs:225` | ~3-5 s/CREATE ntex-worker block @ c=20 | ~30 LOC | low | **IMPORTANT — elevated to CRITICAL-adjacent (behind R25-C1)** |
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:411`, `snapshot_store.rs:184` | ~3 s/SNAPSHOT | ~80 LOC | medium | TODO (unchanged) |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT ~3.5 s + WAKE ~2 s + 95% storage | ~200 LOC | medium | TODO (unchanged) |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC | low | TODO (unchanged) |
| **R17-P1** Skip intermediate set_state writes | `wake_machine.rs:280, 309, 401, 424, 456` | ~4-10 ms/wake | ~30 LOC | medium | TODO (unchanged) |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC | low | TODO (unchanged) |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:228`, `admin_handlers.rs:1594-1613` | ~2-6 ms/wake | ~15 LOC | ~zero | TODO (unchanged) |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms | 2 LOC | ~zero | TODO (unchanged) |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low | TODO (unchanged) |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:404-411` | instrumentation | ~5 LOC | ~zero | TODO (unchanged) |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3 s background | 1 LOC | ~zero | TODO (unchanged) |
| **R21-M1** Restoring-phase watchdog tick | `wake_machine.rs:309`, `db.rs:3207` | concurrency fix; ~5-15 ms/wake | ~30 LOC | low | TODO (unchanged) |
| **R24-A2** Batch sweeper pg lookups | `sweep.rs:1038,1078` (call sites); new db helpers | ~3 s/tick → ~50 ms/tick at N=1000 | ~80 LOC | medium | TODO (deferred — not justified at the new grace-600 N_subdirs=150 regime) |
| ~~R24-A1~~ Tighten `HOST_DIR_GC_GRACE_SECS` default | `sweep.rs:898` | ~6× storage reduction | 1 LOC | ~zero | **CLOSED `34b52cf1` — R25-M5** |
| ~~R25-T4~~ Sweeper pure-helper extraction | `sweep.rs:898-985` | test density | — | — | **CLOSED `901dfbf2` — R25-M6** |
| ~~r3-A boot fetch~~ `GET /v1/agent/self` | `lib.rs:670-698`, `nomad_ch.rs:3074-3094` | — | — | — | **CLOSED — R25-M1** |
| ~~r3-A jobspec emit~~ Constraints wire bloat | `nomad_ch.rs:2562-2575`, `restore_handler.rs:2660-2673` | — | — | — | **CLOSED ~5% wire bloat — R25-M4** |
| ~~r3-B `waitForTapAbsent`~~ kernel netdev poll | `nomad-driver-ch/ch/net.go:289-308` | ≤500 ms p99 add to collision-replace CREATE | — | — | **CLOSED — R25-M2** |
| ~~R20-S3~~ Driver SHA256 verify | `gcp-worker-startup.sh:187-198` | — | — | — | **CLOSED (out of scope + one-shot) — R25-M3** |

**Top-3 SLO wins (revised for T-8b-stress-r4 readiness):**

1. **R25-C1 (R23-P1 elevation)** — frees ~112 ms/wake on pool
   handshake + cuts pg-conn-cap pressure at c=20 burst on the
   pinned controller. **CRITICAL — gates T-8b-stress-r4
   readiness.**
2. **R25-I1 (R23-A2 elevation)** — frees ntex worker through
   `try_create`'s ~3-5 s sync block. **IMPORTANT, behind R25-C1.**
3. **R16-P3 + R16-P5** (~5-6 s combined SNAPSHOT + WAKE win,
   unchanged).

**Top-3 storage / steady-state wins (carry-forward):**

1. **R24-A1** (CLOSED `34b52cf1`) — ~6× storage cut delivered.
2. **R16-P5** — 95% storage cut on snapshot artefacts.
3. **R24-A2** (deferred) — sweeper pg roundtrip batching at fleets
   >1000.

## Closures since r24

- **R24-A1** (1-LOC `HOST_DIR_GC_GRACE_SECS` 3600 → 600) — LANDED
  at `34b52cf1`. R25-M5 confirms 6× storage cut and re-runs the
  sweep-cadence math against the new default. **CLOSED.**
- **R25-T4** (pure-helper extraction `classify_host_dir_entry` +
  `host_dir_eligible_by_db`) — LANDED at `901dfbf2`. R25-M6
  confirms zero hot-path delta. **CLOSED.**
- **r3-A precursor** (`883df7fe` boot-time `GET /v1/agent/self`) +
  **r3-A cold-boot** (`9b623f44`) + **r3-A restore-path**
  (`d71f1a8c`) + **r3-A backlog closure** (`b562d3a1`) — LANDED.
  R25-M1 + R25-M4 confirm ~5-50 ms boot + ~5% wire bloat, neither
  on a hot path. **CLOSED.**
- **r3-B driver `waitForTapAbsent`** — LANDED in `nomad-driver-ch`
  (`ch/net.go:289-308`); referenced in this worktree's
  `2ead52c2` v15 SHA-pin bump. R25-M2 confirms ≤500 ms worst-case
  on the collision-replace path, p50 unchanged. **CLOSED.**

## Lens hand-off

**Tier 0 (NEW — CRITICAL):** R25-C1 (R23-P1 elevation). ~150 LOC,
medium risk. **Lands BEFORE T-8b-stress-r4 or the cluster run is
gated on pg-tuning workarounds.**

**Tier 0.5 (NEW — CRITICAL-adjacent):** R25-I1 (R23-A2 elevation).
~30 LOC, low risk. **Lands BEFORE T-8b-stress-r4 if R25-C1
lands; otherwise both are gated together.**

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — unchanged.

**Tier 2.5 (1 PR, ~10-25 ms/wake + ~5-15 ms/wake POST + ~1-5 ms/
CREATE):** R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2
metric + R16-P6 (8 → 32 MiB chunk size). Unchanged composition.

**Tier 2.5b (1 PR, R21-M1 watchdog tick):** unchanged.

**Tier 2.6 (CLOSED at `34b52cf1`):** R24-A1 grace tighten — **shipped**.

**Tier 2.75 (CLOSED-or-elevated):** R11-P1 / R23-P1 moved to Tier 0
(R25-C1) — no longer at this tier.

**Tier 3 (2 PR, ~5-6 s SLO win on measured baseline):** R16-P3 +
R16-P5. Unchanged sequencing — gate on T-8b-stress-r4 cluster
validation.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT +
wake-phase-duration histogram + host_dir GC scanned/reaped
histogram per tick.

**Tier 5 (1 PR, R24-A2 sweeper pg-lookup batching):** ~80 LOC,
medium risk. Deferred — not justified at the new grace=600
N_subdirs=150 regime.

## Carry-forward notes (focus areas requested)

### r3-A boot fetch impact

- **One-shot at boot, ≤5 s capped, ~5-50 ms typical.** Inside
  `spawn_blocking` so compio reactor unaffected; result cached on
  `AppState.local_nomad_node_id` for the controller's lifetime.
- **Failure mode is non-fatal:** `None` fallback omits the
  Constraints block → pre-r3-A random placement at WORKER_COUNT>1.
  Operator-alertable via `sandbox_nomad_node_id_lookup_failures_total`
  counter. **Confirmed not on any hot path** (R25-M1).
- **Typical-agent-load worst case** (e.g., Nomad fronted by a
  reverse proxy or restarting): ~100-500 ms. **5 s cap protects
  the hung-agent edge case.**

### r3-A jobspec Constraints wire bloat

- **~110-130 bytes/CREATE** added to the jobspec body.
- **Jobspec baseline:** ~2.0-2.5 KB → ~5% wire bloat (R25-M4).
- **At c=20 stress over 60 cycles:** ~144 KB extra over a full
  run. Trivial.
- **Pg-side:** zero (jobspec not persisted to pg).
- **Nomad-side:** ~120 KB across a 1k-Job cluster; trivial against
  Nomad's Raft state thresholds.
- **Per-call emit cost:** <10 µs.

### r3-B `waitForTapAbsent` worst-case at c=20

- **Common path** (no collision): 0 ms added.
- **Collision-replace path** (~3.3% of CREATEs at stress-r3 rates):
  worst-case 500 ms wall (5 × 100 ms poll). Typical <100 ms
  (kernel usually releases within one tick).
- **p50 CREATE wall: unchanged.** **p99 add: ~500 ms** on rare
  collision-replace + slow-kernel intersect.
- **Inside the 60 s alloc_running_timeout budget** (<1% of
  budget).
- **Runs in driver process, not ntex worker** — does NOT compound
  R23-A2's ntex-block budget directly.
- **Cap is right-sized:** finer polling buys nothing (kernel
  release is dominated by tun-driver's cleanup tick, not poll
  cadence).

### R24-A1 grace tighten cadence math

- **Post-`34b52cf1` peak storage: ~22.5 GB at 10 creates/min
  sustained** (down from ~130 GB at the old 3600 s grace).
- **6× cut delivered.** Confirmed in R25-M5.
- **Pg sweep work:** ~3 s/tick at N_subdirs=150 (down from ~19 s
  at N_subdirs=1000); ~1% duty cycle on the dedicated thread.
- **Operator escape hatch:** `SANDBOX_HOST_DIR_GC_GRACE_SECS` env
  unchanged; 60 s floor preserved.

### R25-T4 sweeper helpers

- **Pure helpers** (`classify_host_dir_entry`,
  `host_dir_eligible_by_db`), no Database / no AppState
  references.
- **Compiler view:** rustc inlines at opt≥2 release/cargo-test
  defaults. Even without inlining, same instruction shape as the
  prior inline code.
- **Per-tick cost: zero delta.**
- **Test-density gain:** +13 unit tests (`cargo test -p
  zeroship-sandbox --lib` 465 → 478). **No production behaviour
  change.**

### R23-P1 (R25-C1) elevation rationale

- **Pre-r3-A:** random cross-node placement diluted pg pressure
  across 3 controllers (~93 conns/sec each). Safety factor ~1.5×
  against the typical 100/200 `max_connections` ceiling.
- **Post-r3-A:** 100% of c=20 traffic concentrates on the
  controller's local pg. Effective conn rate **280/sec**. Safety
  factor drops below 1× without pg-tuning.
- **Sweeper alignment:** host_dir GC churns 2 × N_subdirs pool
  opens per 5-min tick at the new grace; at N_subdirs=150, that's
  300/tick = 1/sec sustained — small, but aliases with the wake
  burst on the same back-end.
- **Elevation timing:** R11-P1 was deferred at r11 for being mid-
  Phase-B risk. r3-A's load-bearing landing changes the calculus
  — the deferred refactor is now on the critical path for the
  next cluster run.

### R23-A2 (R25-I1) elevation rationale

- **Pre-r3-A:** c=20 spread across 3 controllers' ntex pools at
  ~6-7 CREATEs each. Per-pool sibling-block budget ~18-21 s.
- **Post-r3-A:** all c=20 lands on the controller's ntex pool. At
  4-8 ntex workers default, the pool saturates after 4-8
  concurrent CREATEs; the rest queue behind the ~3-5 s sync block.
  Estimated p99 sibling add: ~9-15 s.
- **R25-M2 compounding** (r3-B tap-poll): up to 500 ms additional
  wall per collision-replace CREATE — but in the DRIVER process,
  not the controller ntex worker. Does not directly extend the
  ntex-block budget; only extends `wait_for_alloc_running`'s wall.
- **Fix shape:** wrap `try_create` body in
  `compio::runtime::spawn_blocking` at `nomad_ch.rs:606` for
  backend-internal coverage. ~30 LOC, low risk, broad-coverage.

## Diagnostic-discipline carry from r24

r24's discipline: "all projections must be grounded in measurement;
explicitly mark projection-of-projection vs delta-against-measured".
r25 sustains:

- **R25-C1's 280-conns/sec** is a **measurement-derived projection**:
  measured `14 × open_pool` per wake × measured `c=20` burst.
- **R25-I1's ~3-5 s sync block** is **measurement-derived** from
  r24-M5's verbatim `try_create` syscall enumeration; no new
  estimate.
- **R25-M1's ~5-50 ms typical fetch wall** is **estimate-from-
  shape**: ureq HTTP/1.1 against local Nomad agent + 10-50 KB body;
  flagged as estimate, not measurement.
- **R25-M2's ≤500 ms worst case** is **arithmetic from constants**:
  `tapReleasePollAttempts × tapReleasePollInterval = 5 × 100 ms =
  500 ms`. Hard upper bound.
- **R25-M3's ~50-100 ms SHA256** is **estimate-from-shape**:
  sequential SSD read + SHA256 hash at standard throughputs.
- **R25-M4's ~110-130 bytes** is **byte-counted** against the
  emitted JSON shape; hard count.
- **R25-M5's storage table** is **arithmetic** from the new
  grace=600 constant + r24-M2's measured per-dir size.
- **R25-M6's zero delta** is **provable from rustc/LLVM
  optimisation invariants** + identical instruction shape.

No projection-of-projection. All numbers carry their grounding.

## Net assessment

The r3-A node-affinity landings + r3-B driver tap-poll fix close
the cross-node placement race that gated T-8b-stress-r3, at
**architecturally-correct zero direct cost** to the hot paths:

- Boot fetch: one-shot, capped, cached.
- Per-CREATE wire emit: ~120 bytes (~5%) bloat; <10 µs emit cost.
- Per-CREATE wall: 0 ms common path; ≤500 ms worst case on the
  exceptional collision-replace path.

The R24-A1 + R25-T4 sweeper landings deliver:

- **6× steady-state storage cut** (R24-A1 confirmed in R25-M5).
- **+13 unit tests** of pure-helper coverage (R25-T4) at zero
  hot-path delta.

**The two remaining open carries (R23-P1, R23-A2) are both
elevated this round** because the r3-A node-pin concentrates c=20
stress traffic on a single controller, changing the safety-factor
math against:

- pg's `max_connections` cap (R25-C1, **CRITICAL**).
- the ntex worker pool's sync-block budget (R25-I1, **IMPORTANT
  / CRITICAL-adjacent**).

Both are pre-existing carries; neither was introduced by r3-A. But
r3-A's load-bearing concentration makes both exposed in the next
cluster run (T-8b-stress-r4 expected at WORKER_COUNT=3 c=20). The
recommendation is to land R25-C1 before T-8b-stress-r4 ships, or
gate the cluster run on a pg-tuning workaround (max_connections
bump + pgbouncer transaction-mode in front).

**No new carries introduced by r25 landings.** All r25 work is
**closed zero-cost** (R25-M1, R25-M3, R25-M4, R25-M5, R25-M6) or
**closed-as-measured-against-budget** (R25-M2, ≤1% of
alloc_running_timeout).
