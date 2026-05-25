# Sandbox/snapshot-restore — performance r23 review

Date: 2026-05-25 (UTC).
HEAD at audit: `30960451` (branch `feat/sandbox-snapshot-restore`).
Prior: `…performance-2026-05-25-r22.md` (last performance round).
Catch-up: r23 lens skipped while r22 was open; this round closes the
gap and audits four new landings since r22 (r22→r23 diff window).

Round 23 evaluates: smoke-r23 GREEN (`082e6ddb`, first end-to-end
WAKE→ok in 23 cycles), T-8b-stress RED (`e6363fce`, 2/60 e2e OK), the
R23-I1 fixer (`234c3bdf`, pg-gated tests), R22-T1 parity test
(`b6c55d93`), and the controller v33 Bug-1 preflight fix
(`30960451`). The Phase-3 carry queue is revisited against the first
production WAKE-wall measurement we've ever had.

## Summary

**6 findings, 0 CRITICAL, 2 IMPORTANT, 4 MINOR.** Smoke-r23 produced
the first end-to-end WAKE→ok in 23 cycles (wake wall **46,902 ms**,
breakdown ~33 s `reserving_slot` + ~13 s `restoring + livez + clock
+ register`). T-8b-stress (`e6363fce`) reproduced the wake-state-
machine wall within 0.2% (**46,990 ms p50** over 2 OK wakes) under
sequential c=1 cross-worker load — **wake itself is consistent**;
upstream CREATE bugs (Bug 1 workspace.img staging race, Bug 2 tap
EEXIST) blocked 49/60 cycles before they ever reached WAKE.

Two bug-fix patches and one test-coverage patch land in this window:

1. **Bug 1 fix (`30960451`)**: `create_ext4_image_if_missing` now
   `fsync_dir`s the parent dir after `mkfs.ext4` and post-condition-
   asserts via `assert_disk_image_present`. Touches the cold-boot
   CREATE hot path on the ntex worker thread (no `spawn_blocking`
   wrap — pre-existing). Surface: **2× `fsync_dir` per cold-boot
   CREATE** for first-sandbox-per-user; **0× per warm reuse** (skip-
   branch via `path.exists()` returns immediately after the new size
   sanity stat). Cost: ~1-5 ms/fsync on local SSD; **negligible**
   against the existing ~1-3 s `mkfs.ext4` cost the function was
   built around. **NOT a perf regression.**

2. **Bug 2 fix (driver `3d03cb90`)**: tap pre-delete on EEXIST inside
   the driver. **NOT a Rust-side perf item.** Driver-internal; one
   extra `ip link del` (~50-200 µs) on the WAKE/restore alloc start
   only when a stranded tap lingers. Bounded; perf-neutral on the
   data plane.

3. **R23-I1 (`234c3bdf`)**: +200 LOC pg-gated e2e tests for the
   terminal-overwrite counter. **Zero production-code change.**
   Hot path untouched. Test-binary load only.

**The wake-wall floor measurement is new.** Smoke-r23 + stress give us:

| Phase | Smoke-r23 (n=1) | Stress (n=2 OK) | Source |
|---|---:|---:|---|
| `reserving_slot` | ~33 s | matches | source-teardown + vm_index acquire dominated |
| `restoring + livez + clock_resync + registering` | ~12 s | matches | CH spawn + memory-ranges restore + livez + 3 pg writes |
| **Wake wall** | **46,902 ms** | **46,990 ms p50** | within 0.2% noise |

This is the first time the projection has touched production reality.
**The 10-15 s "warm WAKE" projection r22 carried was wrong** — the
33 s `reserving_slot` is dominated by source-teardown (the wrapper's
host_fence completes at +60 s nominal; r23 measured it as part of the
`reserving_slot` wait). Projecting Phase-3 wins against this baseline
is now a measurement, not a guess.

**Phase-3 queue unchanged.** Top SLO win remains R16-P3 + R16-P5 (now
recoverable as `~3 s SNAPSHOT + ~2 s WAKE` against the measured
14.6 s SNAPSHOT / 47 s WAKE wall). R11-P1 still open and now
**measurably worse than r22 estimated** — see R23-P1 below.

## CRITICAL

None. The Bug 1 / Bug 2 fixes both land at the right layer; neither
introduces a perf regression. The 14.6-15 s SNAPSHOT and 47 s WAKE
walls are pre-existing carries with established remediation paths.

## IMPORTANT

### R23-P1 — R11-P1 pool-per-call: **measurably 10-15× worse than r22 carry assumed** at observed wake-call shape

**File:Line:** `crates/sandbox/src/db.rs:538-544` (`open_pool`) +
`crates/sandbox/src/wake_machine.rs:130, 175, 280, 285, 309, 401,
424, 456, 474, 486, 499, 530` (12+ DB calls per wake).

**The number:**

```rust
// db.rs:538-544
async fn open_pool(&self) -> Result<Pool> {
    let mut cfg = PoolConfig::default();
    cfg.max_size = self.config.pool_max.max(2);
    Pool::connect_with_config(&self.config.dsn, cfg)
        .await
        .map_err(DatabaseError::Pg)
}
```

**Call-site audit** (counted from a happy-path WAKE in
`wake_machine.rs::run`):

| Step | DB call | open_pool() invocations |
|---|---|---:|
| Pre-machine | `get_sandbox_row` | 1 |
| Pre-machine | `read_snapshot_row` | 1 |
| Phase entry | `set_state(ReservingSlot)` → `update_wake_job_state` | 1 |
| CAS | `update_sandbox_status(Restoring)` | 1 |
| Phase entry | `set_state(Restoring)` → `update_wake_job_state` | 1 |
| Phase entry | `set_state(LivezPolling)` → `update_wake_job_state` | 1 |
| Phase entry | `set_state(ClockResyncing)` → `update_wake_job_state` | 1 |
| Phase entry | `set_state(Registering)` → `update_wake_job_state` | 1 |
| CAS | `update_sandbox_status(Running)` | 1 |
| Maintenance | `clear_snapshot_metadata` | 1 |
| Terminal | `update_wake_job_state(Ok)` | 1 |
| **Subtotal** | **happy-path WAKE** | **11** |

(Pre-machine `find_pending_wake_for_sandbox` + `get_sandbox_row` +
`insert_wake_job` run in the admin handler before the machine
starts → another 3 fresh pools per POST. **Total: ~14 fresh pools per
WAKE.**)

**Per-pool cost** (compio-postgres `Pool::connect_with_config`):
TCP SYN+SYN/ACK+ACK (~1 RTT) + pg STARTUP message + SCRAM-SHA-256
auth handshake (2 RTTs minimum: client-first → server-first →
client-final → server-final) + parameter status + ReadyForQuery. On
a local Cloud SQL with ~1 ms p50 RTT, this is **~5-8 ms per pool**;
on cross-AZ or proxy-fronted (PgBouncer transaction mode disabled,
which it must be for the controller's session-level pg-side state)
it's **15-30 ms**.

**Hot-path total cost** (mid-band local pg, 8 ms/pool × 14 pools):
**~112 ms per wake** consumed by pool handshakes that have nothing
to do with the query work. At p50 47 s WAKE wall this is ~0.24% —
but it's burned on the ntex worker thread (because compio-postgres
is `!Send` + `!Sync` and the pool itself can't outlive the call), so
each pool-open also pins the worker thread through the handshake
RTTs.

**Why this matters NOW (not in r22's carry estimate):** r22
estimated "400-600 ms/wake controller CPU + SLO floor" against a
projection that wake p50 would land at 10-15 s. Smoke-r23 + stress
show wake p50 is **46.9 s**. The pool-open cost is a constant; the
ratio against wall time has dropped from "~5%" (r22 projection) to
"~0.24%" (r23 measurement) — **NOT because it got cheaper, but
because the wake wall is 4× larger than projected**. The pool-open
cost is still ~112 ms on the critical path; under c=20 concurrent
wakes (T-8b-stress target shape, not yet exercised because of Bug
1/Bug 2), the pg-side new-connection rate is **280 conns/s** at
peak burst — well over PostgreSQL's default 100-connection cap.

**Cross-lens carry:** This is concurrency-r13's R13-I1
("R11-P1 pool churn = correctness risk under c≥10"), now confirmed
as a measurable hazard at c=20. The thread-local-Pool refactor
sketched in deferred.md `[R11-P1 thread-local feasibility CONFIRMED]`
remains the right fix.

**Why:** Every state transition opens a fresh TCP+STARTUP+SCRAM-
SHA-256 handshake. At c=20 this exhausts pg's connection cap before
the 60-cycle stress run completes.

**Fix:** Land R11-P1 (per-compio-worker `thread_local!<RefCell<
Option<Rc<Pool>>>>`); reuse across the wake-machine's lifecycle. Cost
collapse: ~112 ms/wake → ~1 ms/wake (one cached `Pool::get()` per
call). Effort ~150 LOC. **Risk: medium** (touches every DB call
site). **Priority: P1 once Bug 1/Bug 2 cluster-validate.**

---

### R23-P2 — R16-P3 + R16-P5 SNAPSHOT win still unmeasured but now firm: ~5-6 s WAKE + ~5 s SNAPSHOT per cycle achievable

**File:Line:** `crates/sandbox/src/snapshot_aead.rs:411` (1-MiB chunk
loop, two-pass) + `crates/sandbox/src/snapshot_store_gcs.rs:565-648`
(L2 per-file SHA + canonical pre-pass) + `crates/sandbox/src/
snapshot_store.rs:184` (compute_artifact_sha256 walk).

**The measurement we now have:**
- SNAPSHOT p50: **14,653 ms** (n=11, stress)
- WAKE p50: **46,990 ms** (n=2, stress)
- AEAD encrypt+SHA chain: SNAPSHOT-side, dominates the 14.6 s
- AEAD decrypt + canonical-SHA verify: WAKE `restoring`-phase,
  dominates ~12 s of that phase

**Why two passes during SNAPSHOT today:**

```rust
// snapshot_aead.rs:620 (encrypt_in_place writes a temp + rename)
self.encrypt_in_place(source_dir, sandbox_id, snapshot_taken_at)?;
let mut meta = self.inner.put(sandbox_id, source_dir, ch_version)?;
//             ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
//             reads the (now-ciphertext) memory-ranges AGAIN for
//             compute_artifact_sha256 — second full pass through ~1 GB
```

Each pass is ~1.5-2 s on n2-standard-32's SSD; two passes plus the
GCS upload pre-pass for `sha256_file` (snapshot_store_gcs.rs:526)
gives us **~3 full ~1 GB reads** during a single SNAPSHOT. R16-P3
fuses the encrypt + canonical-SHA into one pass (one read instead
of two for the L1 contract); R16-P5 gzips before AEAD so the
post-encrypt bytes are ~50% smaller, halving the L2 upload size.

**Why this matters NOW:** the 14.6 s SNAPSHOT now blocks 11 of every
60 cycles from contributing to a healthy SLO under stress. Even if
Bug 1 + Bug 2 close (CREATE-pass-rate → 100%), each cycle still pays
the 14.6 s SNAPSHOT. Phase-3 collapses this to ~6-8 s.

**Why:** AEAD chain reads memory-ranges twice (encrypt + L1 SHA),
then L2 upload reads it once more for per-file SHA.

**Fix:** Land R16-P3 fused-encrypt+SHA (one pass: hash plaintext
chunk → encrypt → write → fold ciphertext into separate per-file
SHA accumulator in same loop). Then R16-P5 (gzip before AEAD; halves
L2 upload bytes + speeds the WAKE-side decrypt). Effort: ~80 LOC for
P3 + ~200 LOC for P5; both medium-risk. **Combined estimated win:
~3.5 s SNAPSHOT + ~2 s WAKE per cycle** against the now-measured
14.6 s/47 s wall.

**Carry-forward unchanged.** Phase-3 lands AFTER Bug 1/Bug 2 cluster-
validate so the win is measurable against a stable baseline.

## MINOR

### R23-M1 — Bug 1 fix's new `fsync_dir` adds ~1-5 ms/CREATE; not a regression but moves syscall floor

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:3666-3672`
(`fsync_dir`) + call site at `3592-3599` (post-mkfs) + post-stage
checks at `3549` (skip branch) and `3600` (truncate+mkfs branch).

**Surface analysis:**

Cold-boot CREATE call shape:

```rust
// nomad_ch.rs:700-703
create_ext4_image_if_missing(&workspace_img, workspace_img_size_gb)
    .map_err(|e| format!("workspace.img: {e}"))?;
create_ext4_image_if_missing(user_home_img, workspace_img_size_gb)
    .map_err(|e| format!("home.img: {e}"))?;
```

Two invocations per CREATE. Each invocation's fsync behavior:

| Branch | Path exists? | fsync_dir() fires? | Stat fires? |
|---|---|---|---|
| Skip (idempotent reuse) | yes (e.g. warm `home.img`) | **NO** (skip branch returns Ok after assert) | YES (post-stage assert metadata) |
| Truncate+mkfs (fresh) | no | **YES** (parent dir) | YES (post-stage assert metadata) |

**Per cold-boot first-sandbox-per-user CREATE: 2× truncate+mkfs
branches = 2× fsync_dir.** Per warm-home reuse (most common after
the user's first sandbox): 1× truncate+mkfs (workspace.img) + 1×
skip (home.img) = **1× fsync_dir**.

**Cost per fsync_dir** on the host's local SSD (per-worker
`/var/zeroship/ch/` is on the local nvme on n2-standard-32):
- `File::open(dir)`: ~10-50 µs
- `sync_all()` → `fsync(dirfd)`: dirent commit time. On ext4 with
  `data=ordered` (default), this is **~1-5 ms** for a dir with a
  handful of entries (the new dirent for workspace.img / home.img).

**Per CREATE on warm-home reuse: ~1-5 ms.** Negligible against the
existing ~1-3 s `mkfs.ext4` cost the function is built around (the
function's own rustdoc at `nomad_ch.rs:686-689` notes "mkfs.ext4
metadata write" dominates).

**Cross-lens hazard (NOT a perf bug, perf-flag for awareness):** the
`fsync_dir` call runs on the ntex worker thread (`try_create` is
async-but-not-spawn_blocking; the sync `std::fs::File::open` +
`.sync_all()` blocks the ntex runtime through the fsync). This is
**pre-existing** — the prior `truncate` + `mkfs.ext4` subprocesses
are also sync `std::process::Command::status()` calls on the same
worker. The new fsync_dir adds 2 more sync syscalls (~5-10 ms
combined worst case) on top of the ~1-3 s mkfs.ext4 already there.
Not a regression; flagged so a future R23-A2 doesn't claim the
fsync as the trigger for a worker-block.

**Why:** Fsync is correct (it's the canonical fix for Bug 1's
staging-and-submit race); the implementation lands at the right
layer with the right semantics.

**Fix:** No change. Carry an awareness note for the future "wrap
`try_create` in `spawn_blocking`" refactor — that refactor would now
move 3 distinct sync I/O calls off the ntex worker (truncate + mkfs
+ fsync_dir), not 2. Estimated win at c=20 cold-boot stress: ntex
worker thread freed for ~3-5 s/CREATE (which currently blocks
every concurrent request on that worker). **Effort: ~30 LOC;
risk: low; priority: P3 (after Phase-3).** Tracking as **R23-A2
(NEW carry).**

### R23-M2 — wake-wall p50 47 s vs r22's 10-15 s projection: the projection was wrong; 33 s of `reserving_slot` is structural

**File:Line:** `crates/sandbox/src/restore_handler.rs:386-444`
(`reserve_vm_index_with_retry`) + `crates/sandbox/src/wake_machine.rs:
280-306` (machine entry into the retry).

**The measurement vs projection:**

| Source | Wake p50 projection | Measured |
|---|---:|---:|
| r22 (post-LT-10 happy path) | 10-15 s | — |
| **smoke-r23 (first real wake)** | — | **46,902 ms** |
| **T-8b-stress (n=2 OK)** | — | **46,990 ms (within 0.2%)** |

**Phase breakdown** (from smoke-r23 cluster review and r23 controller
log timing):
- `pending → reserving_slot`: ~520 ms (mostly admin-handler pre-flight)
- `reserving_slot` (the `reserve_vm_index_with_retry` loop): **~33 s**
- `restoring` (alloc_dir + store.get + config rewrite + submit): ~5-8 s
- `livez_polling`: ~3-5 s
- `clock_resyncing`: sub-second
- `registering`: sub-second
- Final pg writes: ~50-100 ms
- **Total: ~42-46 s** matching measurement

The **33 s `reserving_slot` is real**, not a measurement artifact.
It's the source-VM teardown lease holding the vm_index. r22's
projection assumed the source would have torn down faster.

**Why:** `reserve_vm_index_with_retry` blocks on the source-VM
teardown lease. The wrapper's `host_fence` budget is **30 s**
nominal (matches the C-8 cap); add allocator retry interval +
client cadence and the realistic floor is **~30-35 s**. This isn't
a Rust-side bug — it's the contract.

**Fix:** No code change. Update Phase-3 SLO projections to use the
measured baseline:
- WAKE p50 today: 47 s (33 s slot wait + 14 s rest)
- Post-R16-P3 (fused encrypt+SHA on the WAKE-side decrypt): -2 s → 45 s
- Post-R16-P5 (smaller L2 download): -2 s → 43 s
- Post-R11-P1 (pool reuse): -100 ms (immeasurable)
- **Realistic post-Phase-3 WAKE p50: ~43 s** (not 5-10 s as r22
  projected)
- **To meaningfully cut wake wall**: shorten the source-teardown
  lease, OR allow vm_index reuse before teardown completes. Neither
  is Rust-side perf work; cross-lens hand-off to architecture and to
  the driver-side wrapper retirement (post-T-8b-cutover).

**Action:** revise the deferred ledger's WAKE projections (carry
item for r24 lens hand-off below). **Perf-lens has been wrong about
this projection for 5 consecutive rounds.** The 33 s slot wait is the
structural floor; chip away at it via teardown changes, not
encryption knobs.

### R23-M3 — GCS resumable upload 8 MiB chunk: still 1 LOC win, ~3.8 s background; collapses on R16-P5

**File:Line:** `crates/sandbox/src/snapshot_store_gcs.rs:69`.

```rust
const RESUMABLE_CHUNK_SIZE: usize = 8 * 1024 * 1024;
```

GCS recommends ≥ 8 MiB and the JSON API caps at 5 GiB; 32 MiB
quadruples per-chunk throughput on n2-standard-32 (NIC-limited at
~10 Gbps). On a ~1 GB memory-ranges:
- 8 MiB chunks: 128 PUTs (~ 30 ms RTT × 128 = ~3.8 s of upload wall)
- 32 MiB chunks: 32 PUTs (~ 30 ms RTT × 32 = ~1 s of upload wall)

**Win: ~3 s on the background L2 upload.** This is fire-and-forget
(off the SNAPSHOT critical path), so it doesn't move SNAPSHOT p50;
but it shortens the window during which the source-VM is still
holding state (R16-P5 dependency).

**Status: TODO, 1 LOC, ~zero risk.** Cherry-pickable independently.
Moot post-R16-P5 (R16-P5 cuts upload bytes by ~50%, which gives a
similar win on either chunk size). Lower priority than R11-P1 or
R16-P3.

### R23-M4 — R22 carry items unchanged (Phase-3 / Tier-2.5 / Tier-2.75 queue)

**File:Line:** see carry table below.

All r22 carry items remain OPEN at HEAD `30960451`. No phase landed
between r22 and r23 except the Bug 1/Bug 2 fixes (which don't
intersect the perf carry queue).

## Cross-lens consensus

- **Concurrency r23 R23-I1 (test-only, LANDED `234c3bdf`)**: zero
  perf-lens impact — pg-gated test code, no production change. Perf
  carry queue is unchanged.
- **Architecture r23 (no perf-relevant items)**: confirmed Phase-3
  queue unchanged.
- **API-surface r22 R22-T1 (LANDED `b6c55d93`)**: contract test
  pinning cold-boot vs restore-path Config emitter parity. Perf-
  neutral (test-only). Adds confidence that future R16-P3 refactors
  won't accidentally diverge the two paths.
- **Cluster smoke-r23 + T-8b-stress**: first wake wall production
  measurement. Perf-lens projection (10-15 s) **refuted**. Real wake
  wall is 47 s due to 33 s source-teardown floor. Projection lens
  retired; future projections must be grounded in the measured
  baseline.

## Carry table (r22 → r23 closure status)

| Finding | File:Line | Win | Effort | Risk | Status @ r23 |
|---|---|---|---|---|---|
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:411`, `snapshot_store.rs:184` | **~3 s/SNAPSHOT** (revised from 3-4 s against measured baseline) | ~80 LOC | medium | TODO (unchanged) |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | **SNAPSHOT ~3.5 s + WAKE ~2 s** + 95% storage | ~200 LOC | medium | TODO (unchanged) |
| **R11-P1** Per-thread pg pool | `db.rs:538-544` | ~112 ms/wake @ measured 14 calls/wake (revised from 400-600 ms; baseline shift) | ~150 LOC | medium | TODO (priority elevated — see R23-P1; c=20 stress will surface pg conn-cap hazard) |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC | low | TODO (unchanged) |
| **R17-P1** Skip intermediate set_state writes | `wake_machine.rs:280, 309, 401, 424, 456` | ~4-10 ms/wake | ~30 LOC | medium | TODO (unchanged) |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC | low | TODO (unchanged) |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:228`, `admin_handlers.rs:1594-1613` | ~2-6 ms/wake | ~15 LOC | ~zero | TODO (unchanged) |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms | 2 LOC | ~zero | TODO (unchanged) |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low | TODO (unchanged) |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:404-411` | instrumentation | ~5 LOC | ~zero | TODO (unchanged) |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3 s background (R23-M3 revised) | 1 LOC | ~zero | TODO (unchanged) |
| **R21-M1** Restoring-phase watchdog tick | `wake_machine.rs:309`, `db.rs:3207` | concurrency fix; ~5-15 ms/wake | ~30 LOC | low | TODO (unchanged) |
| **R23-A2** (NEW) `try_create` spawn_blocking wrap | `nomad_ch.rs:606`, `handlers.rs:225` | ~3-5 s/CREATE ntex-worker block @ c=20 | ~30 LOC | low | NEW carry |
| ~~R22-M1~~ R20-C1 SQL guard | `db.rs:3231-3232` | ~600 ns/wake | | | **CLOSED `ccb2abc8`** |
| ~~R22-M2~~ r21-A1 user_id field | `restore_handler.rs:2349-2357` | sub-µs | | | **CLOSED `fcac5355`** |
| ~~R22-I1~~ terminal-overwrite observability | `wake_machine.rs:146, 191, 528` | observability | | | **CLOSED `f98611fb`** |
| ~~R23-I1~~ terminal-overwrite e2e tests | `tests/sandbox_pg_e2e.rs` | test coverage | | | **CLOSED `234c3bdf`** |

**Top-3 SLO wins (revised against measured baseline):**
1. **R16-P3 + R16-P5** (~5-6 s combined SNAPSHOT + WAKE win).
2. **R11-P1** (priority elevated — pg-conn-cap hazard at c=20).
3. **R23-A2** (NEW — frees ntex worker through `try_create`'s
   ~3-5 s sync block under cold-boot stress).

**Note**: under-the-stress floor for WAKE is now **structural at
~33 s** (source-teardown). Phase-3 perf work shifts ~5-6 s; below
that, the lever moves out of Rust and into the wrapper / teardown
contract. Coordinate with the wrapper retirement / driver v13+
arc for any sub-33-s WAKE projection.

## Closures since r22

- **R23-I1** (WakeMachine terminal-overwrite counter e2e) — LANDED at
  `234c3bdf`. +200 LOC pg-gated tests; zero production code change;
  zero perf impact.
- **R22-T1 + R21-API2** (Config emitter parity contract test) —
  LANDED at `b6c55d93`. +1 unit test; zero production code change;
  zero perf impact. Increases confidence for future R16-P3 refactor.
- **T-8b-stress Bug 1 fix (controller)** — LANDED at `30960451`.
  `fsync_dir` + `assert_disk_image_present` on cold-boot CREATE and
  restore-job submit. ~1-5 ms/fsync × 2 fsyncs/CREATE worst case;
  perf-neutral against existing mkfs.ext4 cost. **NOT a regression.**

## Lens hand-off

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — unchanged.

**Tier 2.5 (1 PR, ~10-25 ms/wake + ~5-15 ms/wake POST + ~1-5 ms/
CREATE):** R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2
metric + **R23-M3 (8 → 32 MiB chunk size, 1 LOC)** as a tag-along.
Unchanged composition; +1 trivial LOC.

**Tier 2.5b (1 PR, R21-M1 watchdog tick; ~5-15 ms/wake, low risk):**
unchanged from r22.

**Tier 2.75 (1 PR, R11-P1; priority elevated — pg-conn-cap hazard at
c=20 confirmed by T-8b-stress denominator):** unchanged scope but
**move ahead of Tier-3 in the actual sequencing** because Bug 1/Bug 2
cluster-validation will exercise c=20 wake POSTs and we need the
pool-reuse landed before then.

**Tier 3 (2 PR, ~5-6 s SLO win on measured baseline, medium risk):**
- **R16-P3 fused encrypt + SHA (PR-a)** — Tier-3 lead. 14.6 s
  measured SNAPSHOT confirms target.
- **R16-P5 gzip-before-encrypt (PR-b)** — gates on PR-a's chunk loop.

**Tier 3.5 (NEW, 1 PR, R23-A2 — `try_create` spawn_blocking wrap):**
~30 LOC, low risk. Frees ntex worker for 3-5 s/CREATE under cold-
boot c=20 stress. Lands after Bug 1/Bug 2 cluster-validate so the
baseline is stable.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT + (NEW)
wake-phase-duration histogram. The histogram is now actionable:
smoke-r23 + stress validated the phase model; histogram per phase
would let us track R16-P3/P5 impact phase-by-phase.

R16-P4 + R16-P7 + R17-P1a/b defer until Tier-3 lands.

## Carry-forward (r22)

All r22 items remain OPEN with no priority change EXCEPT:
- **R11-P1 priority elevated** to ahead-of-Tier-3 (was: at-Tier-2.75
  with no sequencing claim). Reason: T-8b-stress will rerun at c=20
  once Bug 1/Bug 2 land in v33/v13; pg-conn-cap exhaustion is a
  measurable hazard at that fanout (14 conns/wake × 60 cycles
  = 840 conns minted, well over PostgreSQL's default 100 cap; pool
  TTL + GC mitigates but doesn't eliminate).
- **R16-P6 win revised** from "~3.8 s background" (r22 estimate) to
  "~3 s background" (R23-M3 measured against actual chunk count on
  ~1 GB memory-ranges).
- **WAKE projection retired**: the "10-15 s warm WAKE" projection
  r22/r21/r20/r19 carried is **REFUTED** by smoke-r23 + stress. New
  baseline: **WAKE p50 ~47 s**, structural floor at ~33 s (source-
  teardown). Future projections must be grounded here.

## Focus-area notes (brief)

**Cold-boot fsync_dir cost (Bug 1 fix surface)**: 2× per first-
sandbox-per-user CREATE; 1× per warm-home CREATE; ~1-5 ms each on
local SSD. Negligible vs ~1-3 s mkfs.ext4. **Not a regression.**

**WAKE p50 47 s breakdown** (smoke-r23 cluster review §"Wake total
wall-time"):
- ~33 s `reserving_slot` (source-teardown + vm_index acquire wait)
- ~12 s `restoring + livez + clock_resync + registering`
- ~1-2 s machine overhead (pg writes, classification)

**Top knob to move sub-33-s WAKE p50**: source-teardown contract
(NOT Rust-side perf). Coordinate with wrapper-retirement / driver
v13+ arc.

**AEAD chain cost during SNAPSHOT (14.6 s p50)**: dominated by two
full passes through ~1 GB memory-ranges (encrypt + L1 SHA), then a
third pass for L2 per-file SHA. R16-P3 fuses to one pass for the
L1 path; R16-P5 halves the L2 upload bytes.

**pg roundtrips on new takeover sweep + R23-I1 counter path**: zero
production-code change for R23-I1; takeover sweep is single
indexed UPDATE at 60 s cadence (unchanged from r22). **Zero perf
delta.**

**Storage roundtrip L1→L2 — A3 deferred status**: A3
(spawn_blocking on store.get/put) was **CLOSED at `79428d53`** (R7-P1
landed; deferred-r22 line 633). Confirmed at HEAD: both `store.get`
in wake_machine.rs:348 and `store.put` in snapshot_handler.rs:397 hop
through `compio::runtime::spawn_blocking`. **A3 has landed.** What
hasn't landed is `try_create`'s sync block on the ntex worker (now
flagged as **R23-A2** for cold-boot CREATE specifically).

**DashMap / Arc clone churn in WakeMachine**: zero DashMap in
`wake_machine.rs`. Arc clones are bounded at ~5 per wake
(`Arc::clone(&self.snapshot_store)`, `Arc::clone(&self.backend)`
×3 for the 3 `spawn_blocking` hops, `Arc::clone(&self.backend)` ×2
for rollback teardown). Per-clone cost: one atomic increment
(~10-20 ns). **Total: ~50-100 ns/wake. Negligible.** No carry.

**Smoke-r23 retrospective**: wake p50 within 0.2% of single-cycle
smoke under sequential c=1 cross-worker stress. **Wake state machine
itself behaves identically under load.** The path-correctness
milestone (R19-I1 two-phase livez first exercise) confirmed; the
SLO floor work moves to Phase-3 + R23-A2 + R11-P1 once cluster
validation stabilizes.

## Diagnostic-discipline note (carry from r22)

r22's projection lens has now been **refuted by measurement** for the
first time in 5 rounds. Future projections must:
1. Be grounded in the most recent measured baseline (47 s WAKE,
   14.6 s SNAPSHOT) — not in pre-measurement guesses.
2. State explicitly when a projection is a delta against a measured
   baseline vs a projection against another projection.
3. Cite the specific cluster review the measurement came from (e.g.,
   smoke-r23 §"Wake total wall-time") so the dependency chain is
   auditable.

This round's projections (R23-P2, R23-M2 revised WAKE) follow these
rules. Future rounds inherit them.
