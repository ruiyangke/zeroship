# Sandbox/snapshot-restore — performance r13 review

Date: 2026-05-25 (UTC)
HEAD at audit: `42212c5c` (T-8b-smoke-retry-r4 cluster-result commit;
two commits ahead of the perf-relevant `94a8a043` R12-P1 closure).
Round 13 of N. Read-only.

Static review. T-8b-smoke-retry-r4 (cluster cycle r4) **landed a real
single-cycle CREATE timing through the Go nomad-driver-ch v4 for the
first time** — **CREATE p50 = 6494 ms** (1/1). SNAPSHOT still failed
inside `TieredSnapshotStore::put` (Bug C-3 — `compio::runtime::spawn_
blocking` called from inside a `spawn_blocking` worker thread). No
wake-path timing yet; SNAPSHOT panicked before the L1 put returned.

## Summary

**0 new perf findings** this round. All r12-era levers stand; R12-P1
landed at `94a8a043` (BufReader on `download_to_disk` read side). The
substantive r13 deliverables are **observability** updates against the
new cluster data point and a **reclassification audit** of R11-P1 in
light of the concurrency-r13 R13-I1 reframe (pool-churn is now a
correctness-class cliff at c≥10, not just a perf nuisance).

Key shifts since r12:

- **R12-P1 CLOSED** (`94a8a043` 2026-05-25) — `BufReader::with_capacity
  (1 << 20, r.into_reader())` now wraps the `ureq::BodyReader` on the
  download path. `std::io::copy` enters its `BufferedReaderSpec`
  specialization (BufRead + Write); read(2) count on a 1 GB body drops
  from ~131072 (8 KiB) → ~1024 (1 MiB). Symmetric with R11-P2's write-
  side fix.
- **R11-P1 reclassification verified** — per concurrency-r13 [R13-I1],
  the pool-churn pattern is no longer a pure perf lever. At
  c=20 concurrent wakes the eager `min_idle=2` opens exceed PG's
  default `max_connections=100`. The PG-conn-explosion calculus is
  worked through in § "R11-P1 reclassification — conn-explosion
  calculus" below.
- **Cluster-r4 CREATE 6494 ms** — first end-to-end-through-the-Go-
  driver CREATE timing. Compared against the wrapper-based baseline
  (`cluster-2026-05-24-r1` 5295 ms, `cluster-2026-05-24-r2` 5239–5339
  ms), the Go-driver path adds ~1200 ms / ~23% to CREATE. Work-up in
  § "Cluster-r4 CREATE 6494 ms baseline" below.
- **C-3 root-cause perf-side audit** — the panic location is
  `snapshot_store_gcs.rs:1096`, inside `TieredSnapshotStore::put`'s
  detached L2 upload kickoff. The L1 put already completed; this is
  the fire-and-forget upload kickoff that should never have been
  initiated from a `spawn_blocking` thread. Snapshot p50 cannot be
  measured until C-3 lands — but the perf-side analysis of the path
  it would have taken is in § "C-3 perf-path audit" below.
- **R12-I1 wake-path `TaskDriverMode`** — confirmed zero perf cost.
  Sub-µs sync env read + ~14 short String allocs; documented in r12
  Appendix; no shift to the wake-path budget.

No new CRITICAL/MINOR perf findings. All r12/r11/r10/r9
carry-forwards unchanged.

## R12-P1 closure verification

Closure landed `94a8a043` (per brief; verified). Source at
`crates/sandbox/src/snapshot_store_gcs.rs:454-456`:

```rust
let mut writer = std::io::BufWriter::with_capacity(1 << 20, f);
let mut reader = std::io::BufReader::with_capacity(1 << 20, r.into_reader());
std::io::copy(&mut reader, &mut writer)?;
```

Both sides now buffered → `std::io::copy` selects
`BufferedReaderSpec::copy_to` which loops:
```
loop {
    let buf = reader.fill_buf()?;
    if buf.is_empty() { break; }
    writer.write_all(buf)?;
    reader.consume(...);
}
```
No 8 KiB scratch buffer, no intermediate memcpy. On 1 GB download:
- read(2) syscalls: ~1024 (was ~131072)
- write(2) syscalls: ~1024 (R11-P2 already landed)

Code-derived syscall-overhead ceiling on the L1-miss wake-path
download: ~200–400 ms eliminated. Bounded below by network throughput
and ureq's TLS-frame buffering; the actual delta on a fast LAN is
much smaller. Pairs with the read-side counterpart R11-P3 already
applied to `sha256_file` / `canonical_artifact_sha256`.

## R11-P1 reclassification — conn-explosion calculus

The concurrency-r13 R13-I1 finding promotes R11-P1 from "perf only"
to "correctness-class at c≥10". From the perf side, here is the
per-wake connection arithmetic against the defaults this crate ships:

| Layer | Value | Source |
|---|---|---|
| `PoolConfig::min_idle` default | **2** | `crates/compio-postgres/src/pool.rs:73` |
| Eager open-on-construct | `min_idle.max(1)` = **2** conns/pool | `pool.rs:265-283` (`connect_with_retry` × N) |
| `Database::pool_max` default | **16** (env `SANDBOX_PG_POOL_MAX`) | `db.rs:362` |
| `open_pool` calls per wake (do_restore_inner) | **5** | `db.rs:1404, 1447, 1478, 1509, 2353` reachable from restore_handler |
| pg conns opened per wake (cold) | 5 × 2 = **10** | derived |
| Default PG `max_connections` | **100** | postgres default |

So a single wake eagerly opens **10 fresh pg conns** (each pool
ctor pays 2 × `connect_one` = 2 × TCP 3WHS + STARTUP + auth +
SET application_name, etc.) then drops them when the future
completes (Pool drop → PoolEntry drop → Client drop → Connection
task exits → server side reaps).

At c=N concurrent wakes:

| c (concurrent wakes) | Inflight pg conns | vs PG default 100 | Headroom |
|---|---|---|---|
| 1 | 10 | 10% | 90 |
| 4 (perf-r8 baseline) | 40 | 40% | 60 |
| 8 (T-8b cutover target) | 80 | 80% | 20 |
| 10 | **100** | 100% | **0** |
| 16 | 160 | **>100%** | **OVER** |
| 20 (stress label) | 200 | **>100%** | **OVER** |

**At c≥10 we exhaust the default PG conn budget on the wake path
alone**, with zero budget left for:

- `start_heartbeat_task` (`lib.rs:1072`) — periodic pg writes;
- `spawn_takeover_task` (`lib.rs:1283`) — periodic CAS sweeps;
- `spawn_transient_state_takeover` (`sweep.rs:227`) — open_pool per
  tick;
- `spawn_idle_snapshot_sweep` (`sweep.rs:563`) — open_pool per tick;
- Snapshot-path db calls (heartbeat, post-snap update);
- Rollback-path `update_sandbox_status` (`restore_handler.rs:299`) —
  itself reaches for a fresh pool.

**Rollback-path compound** (per R13-I1): if the primary `open_pool`
hits `FATAL: too many clients`, the wake returns
`RestoreHandlerError::Database(Pg(...))` → rollback closure
(`restore_handler.rs:268-313`) → `update_sandbox_status` → its OWN
`open_pool` → also fails → row wedged in `Restoring` until next sweep
notices. The cliff is structural: failing the primary creates
demand for additional pool opens (rollback) that compete for the
same starved budget.

This is **not currently measurable** in cluster smoke (T-8b ran at
c=1, snapshot panicked at c=1). But it is now a hard correctness
gate for any cluster cycle at c≥10. The deferred-tracker's "correct,
just slow" framing at `db.rs:494-509` understates this — promote
the comment block to flag the conn-budget cliff explicitly as part
of the eventual R11-P1 fix landing.

**Per-wake handshake savings if thread-local lands (unchanged from
r11)**: 4 of 5 PG handshakes amortized → ~40–160 ms/wake median +
~2 conns/wake instead of 10 (under steady-state with the same Pool
warm). The conn-explosion side effect collapses entirely once Pool
becomes per-thread (each compio worker holds at most 1 warm pool
per DSN, eager-opened once at first use).

## Cluster-r4 CREATE 6494 ms baseline

First cluster cycle through the Go nomad-driver-ch v4 (gitSHA
`ec6a1de4`, descendant of `f521eb21` C-2 fix). Single sample. Source:
`docs/reviews/sandbox-snapshot-restore-cluster-2026-05-25-T8b-smoke-
r4.md:105`.

```
create p50/p95/p99/max: 6495 / 6495 / 6495 / 6495 ms
```

Wrapper-based reference baselines (pre-driver-v4, pre-C-1 series):

| Cycle | Path | CREATE p50 (c=1, N=1) | Source |
|---|---|---|---|
| `cluster-2026-05-24-r1` | wrapper-based (bash `nomad-vm-wrapper.sh`) | **5295 ms** | r1:314 |
| `cluster-2026-05-24-r2` (smoke segment) | wrapper + rootfs-pull fix | **5239–5339 ms** | r2:418, 730 |
| **`T-8b-smoke-retry-r4`** | **Go driver v4 (ChPlugin task driver)** | **6494 ms** | T8b-smoke-r4:105 |

**Delta: +1199 ms** (≈ +23%) wrapper → Go driver.

Time breakdown — code-derived, NOT measured (the driver hasn't
emitted phase timings yet):

| Phase | Estimate | Mechanism |
|---|---|---|
| `submit_create_job` to Nomad (HTTP) | 50–150 ms | `ureq::post` to Nomad |
| Nomad alloc placement → `running` | 1.0–2.0 s | Nomad scheduler |
| Driver `StartTask` → CH spawn (NEW in v4) | 200–400 ms | Go driver materializes rootfs, stat-checks disks, then `cloud-hypervisor` exec |
| `cloud-hypervisor` boot (kernel + initrd + vsock + disk attach) | 1.5–2.5 s | CH bootstrap |
| Sandbox agent `/livez` poll loop (`std::thread::sleep(250ms)`) | 500–1500 ms | poll cadence on `RealCreateBackend` |
| HTTP round-trip back to client | 10–50 ms | ntex |

Wrapper baseline (5.3 s) skipped the driver-side rootfs
materialization (the bash wrapper did `cd $ART_DIR` in-process before
exec'ing CH). Driver v4 reproduces that effect by walking
`$ZSBX_ARTIFACT_DIR`, stat-checking every disk path, and copying or
symlinking the rootfs into place pre-flight. Net new cost: the
driver-side stat + rootfs setup, plausibly 200–400 ms; the rest of
the delta is uncategorized (one sample, sampling noise wide).

**Net read**: Go driver CREATE costs ~1200 ms over the wrapper
baseline. Within the historical ~10 s CREATE budget. The R11-P1
pool-churn cost lives inside the CREATE path too (the create handler
issues `insert_sandbox` + status updates — let me grep this), so
the per-create headroom against the ~10 s SLO is c=1 only; at c≥8
the same conn-cliff applies on CREATE as on wake.

**Cannot derive wake-path data point** — SNAPSHOT failed pre-WAKE
(see § "C-3 perf-path audit").

## C-3 perf-path audit

Panic location: `compio-runtime-0.11.0/src/runtime/mod.rs:119:13: not
in a compio runtime`. Returned to client as:

```
{"error":"snapshot_store_failed","message":"snapshot store error"}
internal: "snapshot store: snapshot I/O error: spawn_blocking panic: Any { .. }"
```

**Root cause (perf-side)**: `crates/sandbox/src/snapshot_store_gcs.rs:
1096` inside `TieredSnapshotStore::put`:

```rust
impl SnapshotStore for TieredSnapshotStore<L1, L2> {
    fn put(&self, ...) -> Result<...> {
        let meta = self.l1.put(...)?;            // L1 already landed
        let l2 = self.l2.clone();
        ...
        compio::runtime::spawn_blocking(move || { // <-- PANIC HERE
            l2.put(&sandbox_id, &artifact_path, ...)
            ...
        }).detach();
        Ok(meta)
    }
}
```

Call stack at panic:

```
1. snapshot_handler.rs:397   compio::runtime::spawn_blocking(|| {
2.                              store_clone.put(...) // TieredSnapshotStore::put
3.                              ↓
4.                              self.l1.put(...) ← ok (returns meta)
5.                              ↓
6. snapshot_store_gcs.rs:1096   compio::runtime::spawn_blocking(...)
                                ↑
                                runs on a `spawn_blocking` worker
                                thread, NOT a compio runtime thread →
                                no thread-local runtime handle →
                                PANIC
```

**Perf-path audit (what the path WOULD have taken)**:

Step 4 (L1 put on snapshot_store.rs:225) was the part that ran. Steps
5+ never executed; here is what they would have done:

1. **L1.put (LocalDiskSnapshotStore)**: `compute_artifact_sha256` over
   the staged artifact = 1 read of ~2 GB (R5-P1 BufReader in place);
   then `std::fs::rename` of 3 files (~µs each). **Time: ~1.0–2.0 s
   bound by SSD read bandwidth.**
2. **AEAD wrap (if AeadSnapshotStore in chain)**: `encrypt_in_place`
   = 1 read + 1 write of memory-ranges (~2 reads + 1 write total of
   ~1 GB plaintext + ~1 GB ciphertext on R10-P5 unbuffered file IO).
   **Time: ~1.5–3.0 s under AEAD-active.** Currently NOT in the
   cluster smoke (the controller log shows `snapshot_store: AEAD
   DISABLED — guest RAM plaintext on disk + GCS (kek env unset)`),
   so AEAD wrap is not on the path here.
3. **L2 detached upload (TieredSnapshotStore step that panics)**:
   would read the L1 staged ~1 GB memory-ranges, re-SHA it
   (R10-P4 — duplicate hash work), then `upload_resumable` in 8 MiB
   chunks via `ureq::put`. Fire-and-forget; doesn't block the
   snapshot RPC return. **Time on cold L2: 10–60 s depending on the
   GCS pipe; doesn't sit on the wake/snapshot p50.**

**Conclusion**: even with C-3 fixed, the wake-path AEAD-active
budget below stands. The cluster cycle that triggered C-3 was AEAD-
disabled (`kek env unset` in the controller v19 build), so a cluster
wake measurement against C-3-fixed will still be AEAD-disabled and
should land in the **AEAD-disabled** half of the budget table
(faster than AEAD-active by ~1.0–2.0 s per the R9-P1 second-write
sub-cost).

**One-line fix for C-3** (out of perf scope, but worth flagging):
`TieredSnapshotStore::put` should NOT call `compio::runtime::spawn_
blocking` directly. Either:
- pass the compio runtime handle in via the TieredSnapshotStore
  constructor (held as a `compio::runtime::RuntimeHandle` clone), or
- do the fire-and-forget upload via a `std::thread::spawn` (escape
  the compio runtime requirement entirely; the L2 upload is purely
  sync std::io + ureq, no compio I/O), or
- restructure: have the snapshot_handler do the L2 spawn at the
  layer that owns the runtime (after L1.put returns), keeping
  `TieredSnapshotStore::put` purely synchronous.

The 3rd is cleanest and decouples the store trait from compio. Out
of perf scope; concurrency review may pick it up.

## R12-I1 wake-path TaskDriverMode — perf cost = 0

Verified zero. The R12-I1 fix at `restore_handler.rs:1116-1118`
adds a single `task_driver_mode_from_env()` call inside the existing
`spawn_blocking(submit_restore_job)` closure. The fn body:

```rust
fn task_driver_mode_from_env() -> TaskDriverMode {
    match std::env::var("SANDBOX_TASK_DRIVER").as_deref() {
        Ok("ch_plugin") => TaskDriverMode::ChPlugin,
        _ => TaskDriverMode::RawExec,
    }
}
```

Sub-µs sync env table read. The ChPlugin branch of
`build_restore_nomad_job_json` adds ~14 short String allocations
(verified r12 § "Allocation-heavy paths"). Per-restore total: <50 µs.
No measurable shift in the wake budget; below the flagging
threshold.

## R10-P1 verify still open — 5-pass memory-ranges reads (LOCAL+L2 PUT)

Re-traced via Read of the put path:

| Pass | Function | File:line | Read 1 GB memory-ranges? |
|---|---|---|---|
| 1 | `AeadSnapshotStore::encrypt_in_place` reads plaintext | `snapshot_aead.rs:411-453` | YES (1 GB read) |
| 2 | `AeadSnapshotStore::encrypt_in_place` writes ciphertext | `snapshot_aead.rs:445-446` | (write 1 GB, not a read) |
| 3 | `canonical_artifact_sha256` (TieredSnapshotStore::put pre-pass) | `snapshot_store_gcs.rs:585, 991` | YES (1 GB read) |
| 4 | `sha256_file` (per-file pass for x-goog-hash) | `snapshot_store_gcs.rs:597, 526` | YES (1 GB read) |
| 5 | `upload_resumable` streams chunks | `snapshot_store_gcs.rs:354-369` | YES (1 GB read) |
| **Total disk reads of memory-ranges on AEAD-active snapshot put** | | | **4 reads + 1 write** |

(R9-P3 / R10-P1 in earlier reviews used "5 passes" inclusive of the
write pass; the disk-read count is 4 + write of 1.)

Plus the LOCAL `compute_artifact_sha256` if `LocalDiskSnapshotStore`
is the inner store (`snapshot_store.rs:184`). When the chain is
`AeadSnapshotStore<TieredSnapshotStore<LocalDiskSnapshotStore,
GcsSnapshotStore>>`, the canonical SHA is computed twice — once by
`LocalDisk.put` (pass 1) and once again by `Gcs.put` (pass 2)
— a known R10-P4 redundancy.

**R10-P1 status: OPEN.** Nothing has closed any of the passes
between r10 and r13. R11-P2/P3 added buffering on the write side
(and r12 R12-P1 on the GCS download read side), but the pass-count
itself is unchanged.

Fusable architecture: a streaming pipe (single producer reads
memory-ranges; tees to AEAD encryptor + Sha256 hasher + GCS
resumable upload) would fold these 4 reads → 1 read, at the cost of
re-architecting the trait surface. Estimated wall: 1.5–2.5 s
saved/snapshot at AEAD-active 1 GB memory-ranges on a single
SSD stream — but architectural lift, deferred.

## R10-P5 BufWriter on AEAD output — sibling of R11-P2/R12-P1, still OPEN

Brief asked specifically. Confirmed:

- `snapshot_aead.rs:403` — `let mut dst = std::fs::File::create
  (&temp_path)?;` (encrypt_in_place dest, raw File, no BufWriter)
- `snapshot_aead.rs:529` — `let mut dst = std::fs::File::create
  (target_path)?;` (decrypt_to dest, raw File, no BufWriter)
- Both call `dst.write_all(...)` per chunk; chunks are
  `CHUNK_PLAINTEXT_LEN = 65536` (write side encrypt) or AEAD chunk
  ciphertext sizes (decrypt side, ~65552 each). Per 1 GB memory-
  ranges: ~16384 write(2)s of 64 KiB.

R11-P2 already proved the win on the analogous GCS download path
(8 KiB → 1 MiB via BufWriter, ~131k → ~1024 writes). The AEAD
write path is 64 KiB raw — 2× less syscall-amplified than the
unbuffered 8 KiB io::copy default, but still 16× off the 1 MiB
BufWriter ceiling.

Code-derived ceiling per AEAD pass: ~16384 writes × ~1-3 µs syscall
overhead = ~50–150 ms theoretical per 1 GB. Two AEAD passes per
wake (encrypt put + decrypt wake) = ~100–300 ms total. Symmetric
to R12-P1's read-side wrap.

**R10-P5 status: OPEN.** No closure has landed since r10.

**One-line fix (mirror of R11-P2)**: wrap both `dst` handles in
`std::io::BufWriter::with_capacity(1 << 20, f)`; emit the final
`f.sync_all()` against the inner file after `BufWriter::into_inner`
returns ownership (same pattern R11-P2 uses at GCS download).

## R10-P1 fusable architecture — unchanged carry-forward

(see § "R10-P1 verify still open" above; no closure)

## Detached-spawn audit — no regressions

Re-scanned `compio::runtime::spawn(\b` and `spawn_blocking\b` in
`crates/sandbox/src/**/*.rs`. Census unchanged from r12:

- 11 sites of `compio::runtime::spawn` — all pure-async bodies (no
  sync syscalls); audited individually in r10/r11/r12.
- N sites of `spawn_blocking` — all wrap sync I/O (ch.pause /
  ch.snapshot / store.get / store.put / submit_restore_job /
  wait_for_livez / clock_resync). Audited r10.
- **NEW concern surfaced by C-3**: `TieredSnapshotStore::put` calls
  `compio::runtime::spawn_blocking` from inside a synchronous trait
  fn (`SnapshotStore::put`). The trait contract per
  `snapshot_store.rs:99` says callers MUST wrap put/get in
  `spawn_blocking`. So `TieredSnapshotStore::put`'s inner spawn fires
  from a `spawn_blocking` worker thread → no compio runtime → panic.
  This isn't a perf misuse per se (the panic happens before any I/O
  cost is paid), but it IS a layering-violation that the trait
  invariant prohibits. Out of perf scope; concurrency review.

## Wake-path latency budget (updated for r13)

Calibrated against cluster-r4 single-sample CREATE 6494 ms (the only
real cluster data point in 5 cycles).

Note on calibration: the cluster cycle that produced this was
**AEAD-disabled** (`kek env unset`). So the AEAD-active component
of the wake-path budget cannot be directly calibrated against this
sample. What we CAN calibrate is the controller-side overhead OF
the create/restore handlers — `submit_restore_job` and `wait_for_
livez` share their internals (Nomad submit + alloc-status poll +
livez poll) with `submit_create_job` / `wait_for_livez_create`. The
single-cycle CREATE data point lower-bounds the equivalent
controller-side wake overhead.

**Cluster-r4 CREATE 6.5 s breakdown** (code-derived, not measured):
- Nomad submit + alloc placement: ~1.5 s
- Driver `StartTask` + rootfs materialization + CH spawn: ~2.5 s
- VM agent `/livez` post-boot: ~1.5 s
- Sandbox handler overhead + db inserts + HTTP: ~1.0 s

**Wake-path equivalent** has the SAME submit/poll structure
(`submit_restore_job` + `wait_for_livez` mirror the create path) PLUS
the `store.get` + AEAD decrypt + clock_resync work. Calibrated
estimate per row:

| Component | Estimate | Source | Calibration |
|---|---|---|---|
| `submit_restore_job` (spawn_blocking) | ~2.0–3.5 s | `restore_handler.rs:508-524` + `:1450-1508` | mirrors CREATE submit + alloc + start |
| `wait_for_livez` (spawn_blocking) | ~1.0–3.0 s | `restore_handler.rs:533-542` + `:1518-1537` | mirrors CREATE livez wait |
| `store.get` AEAD-disabled (L1 hit hard-link, ~1 GB read for SHA verify) | ~0.5–1.5 s | `snapshot_store.rs:225` | bounded by 1× memory-ranges SSD read |
| `store.get` AEAD-active path (hard-link to stage + 1 GB decrypt-write to target) | ~1.0–2.0 s | `snapshot_aead.rs:633-678` (R9-P1) | second 1 GB write to target |
| GCS download (L1 miss path; R12-P1 + R11-P2 closed) | ~0.8–2.0 s | `snapshot_store_gcs.rs:438-465` (R11-P2 + R12-P1 closed) | wall-bound by GCS pipe + LAN |
| `clock_resync` (spawn_blocking; /dev/urandom + signed POST) | ~0.05–0.15 s | `restore_handler.rs:1582-1668` | unchanged |
| `register_restored` + state.write insert | <0.01 s | `nomad_ch.rs:1659-1690` | unchanged |
| pg awaits (5 fresh handshakes per R11-P1) | ~0.05–0.2 s | R11-P1 OPEN, now corr-class per R13-I1 | bounded by 5× TCP+STARTUP |
| ~32 fresh ureq calls × 1-3 ms TCP 3WHS | ~0.03–0.10 s | R10-P6 OPEN | unchanged |
| Post-store.get diagnostic 3 sync metadata + format! | ~0.0001–0.003 s | `restore_handler.rs:443-458` | unchanged |

**Calibrated estimated wake p50 (AEAD-disabled, L1 hit)**: ~4.4–9.5 s.
**Calibrated estimated wake p50 (AEAD-active, L1 hit)**: ~4.9–11.0 s.

The CREATE 6.5 s data point sits at the high end of the
submit+livez summed estimate (~2.5–4.5 s) plus the create handler's
db/HTTP overhead (~1–2 s) — total ~3.5–6.5 s; the measured 6494 ms
matches the upper bound. **This validates the ~2.0–3.5 s estimate
for submit_restore_job AND the ~1.0–3.0 s estimate for wait_for_
livez** sat the high end on a 1-worker fleet, n2-standard-32, cold
network.

Wake will be slower than CREATE by the AEAD decrypt-write cost
(~1.0–2.0 s when active) + the store.get SHA-verify cost (~0.5–1.5 s)
+ the clock_resync cost (~50–150 ms) − the create's db-side cost
that wake doesn't repeat. **Cluster-r4-equivalent wake (AEAD-
disabled, single-cycle)** would land at ~7.5–10.5 s. With AEAD
active, ~8.5–12.5 s.

Historical baseline (`perf-r8` c=4): wake p50 9.2 s. The cluster-
calibrated wake p50 estimate is consistent with the perf-r8 c=4
measurement (which was at controller scale, not cluster).

The R9-P1 fix (eliminate the second 1 GB write on AEAD-active wake)
remains the single largest edit that could move the AEAD-active p50
into the ~3.5–7.0 s range. Pairs with the R11-P1 pool-churn fix to
shave another ~40–160 ms. Cluster cycles can't validate either until
C-3 lands.

## Carry-forward (still open from r12 / r11 / r10 / r9)

| Finding | Status | File:line |
|---|---|---|
| **R11-P1** Every `Database` method opens fresh pg Pool | OPEN (reclassified corr-class per R13-I1) | `db.rs:492-516`; arch-confirmed thread-local recipe (r12) |
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

## Closed since r12

- **R12-P1**: BufReader on `download_to_disk` read side — landed
  `94a8a043` (`crates/sandbox/src/snapshot_store_gcs.rs:454-456`).
  Verified.

## Ranked next-biggest perf lever (updated for r13)

1. **Hoist `Database` Pool to per-thread `Rc<Pool>`** (R11-P1):
   reclassified corr-class per R13-I1 — same fix closes both the
   perf nuisance (4× PG handshake savings per wake) AND the c≥10
   conn-explosion cliff. Promoted from "performance" to
   "concurrency-correctness". The largest single-edit lever.
2. **Eliminate the second 1 GB write on AEAD-active wake** (R9-P1):
   ~0.5–1.5 s/wake saved; pairs with the c=N SSD-contention
   concern. AEAD-disabled cluster cycles don't exercise this path
   so cluster validation deferred.
3. **Fix C-3** (out of perf scope but pre-req for any cluster
   wake/snapshot measurement): `TieredSnapshotStore::put` must not
   call `spawn_blocking` from inside a `spawn_blocking` worker.
4. **Fuse encrypt + canonical-SHA + L2-side SHA into the streaming
   pipes** (R10-P1 + R10-P4 + r9 #2): ~1.5–2.5 s/snapshot saved at
   c=N stress.
5. **`encrypt_in_place_detached` + `decrypt_in_place_detached`**
   (R10-P3): plausible 100–400 ms per AEAD round-trip;
   measurement-dependent.
6. **Cached `ureq::Agent`** (R10-P6 / r9 #3): ~30–100 ms/wake.
7. **`BufWriter` on AEAD encrypt/decrypt** (R10-P5): ~100–300 ms
   ceiling per AEAD round-trip; sibling of R11-P2 / R12-P1 still
   open.
8. **Sweep allocation cleanup** (R11-P4): sub-ms; code-quality
   more than perf.

## Notes on focus-area questions

**(R13-I1 reclassification verified)**: confirmed at
`compio-postgres/src/pool.rs:73` (`min_idle: 2`) +
`pool.rs:265-283` (eager `connect_with_retry` × `min_idle.max(1)`
on `connect_with_config`). 5 open_pool calls per wake × 2 conns
= 10 fresh conns/wake. PG default `max_connections = 100`.
**c=10 → conn-budget exhausted; c≥10 → wake-path conn cliff.**
The deferred-tracker note frames this as "correct, just slow" —
the framing is incorrect at scale; promote to corr-class. Worked
through in § "R11-P1 reclassification — conn-explosion calculus".

**(Cluster-r4 CREATE 6494 ms baseline)**: Go-driver-v4 path is
~1200 ms (~23%) slower than the wrapper baseline (5295 ms r1;
5239–5339 ms r2). Code-derived breakdown attributes the delta to
driver-side rootfs materialization (stat + setup, ~200–400 ms) +
the rest as wide single-sample noise. Within historical ~10 s SLO
budget. § "Cluster-r4 CREATE 6494 ms baseline" carries the table.

**(C-3 root cause perf-side audit)**: panic at
`snapshot_store_gcs.rs:1096` — `TieredSnapshotStore::put` calls
`compio::runtime::spawn_blocking` for the L2 fire-and-forget
upload kickoff. The caller is the snapshot_handler's own
`spawn_blocking(store.put)` worker thread (no compio runtime in
TLS), so the nested spawn fails the "in compio runtime" assertion.
**Trait-contract violation, not a perf bug per se.** Wake-path
AEAD-active budget table refreshed against the cluster-r4 single-
sample calibration in § "Wake-path latency budget (updated for
r13)".

**(R12-I1 wake-path TaskDriverMode)**: zero perf cost. sub-µs env
read + ~14 short String allocs; confirmed unchanged from r12.

**(R10-P1 verify still open)**: yes, all 4 passes (reads) of
memory-ranges on AEAD-active put still in place. R5-P1 / R11-P3
buffering covers the syscall amplification on each pass
individually; the pass count itself hasn't moved. Architectural
streaming-pipe refactor pending.

**(R10-P5 BufWriter on AEAD output)**: still OPEN. Both
`encrypt_in_place` dst (`snapshot_aead.rs:403`) and `decrypt_to`
dst (`:529`) write to raw `File`. ~16384 write(2)s per 1 GB at the
64 KiB chunk granularity. Sibling of R11-P2; one-line fix mirrors
the GCS download write.
