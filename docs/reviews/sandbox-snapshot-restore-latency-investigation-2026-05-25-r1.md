# Sandbox Wake-Path Latency Investigation — 2026-05-25 r1

**Scope**: Read-only code analysis of the restore/wake path.  
**Baseline**: c=1 smoke from `stress-cutover-c4x5.md` (p50 per-cycle ~30s, wake p50 ~50.7s from T8b task description).  
**Author**: automated analysis pass.

---

## 1. Latency Budget Table

The wake path flows through three layers: the **controller** (Rust/compio), the **Nomad driver** (Go), and **CH** itself (native restore + VM boot). Phase boundaries and known wall contributions:

| Phase | Owner | Measured / Expected Wall | What it spends time on |
|---|---|---|---|
| **CAS + DB reads** | controller/pg | ~50–200 ms | `read_snapshot_row` (1 query) + `update_sandbox_status` CAS to `restoring` (1 write). Loopback pg — fast in prod. |
| **`store.get` (artifact staging)** | controller (spawn_blocking) | unknown, likely 100 ms–2 s | Hardlinks 3 files (state.json, config.json, memory-ranges) from the local snapshot store into the alloc_dir. On a reflink-capable FS this is O(1); on a copy fallback (~1 GB memory-ranges) this is IO-bound. SHA-256 verify runs first: streams all artifact bytes (~1 GB) through sha2 with 1 MiB BufReader. |
| **`submit_restore_job` (Nomad + alloc_running)** | controller (spawn_blocking) | ~2–5 s (typical) | POST `/v1/jobs` to Nomad (~50 ms RTT), then `wait_for_alloc_running_blocking` polls every 250 ms until the driver's `StartTask` reaches `running` status. Includes driver's `startTaskRestoreBranch` wall time (see below). |
| **`startTaskRestoreBranch` (driver)** | Nomad driver (Go) | ~3–20 s | Steps 1–6: validate snapshot dir, read+rewrite config.json, symlink artifacts, stage rootfs via `stageRootfsForRestore` (FICLONE reflink O(1) or byte-copy ~200 ms), OFD lock probe (usually instant on v24+), tap setup, spawn CH with `--restore`, poll API socket (`defaultAPISocketPollTimeout=60s`, `defaultAPISocketPollInterval=100ms`), `ch-remote resume`. |
| **CH `--restore` memory deserialisation** | cloud-hypervisor | **~10–40 s** (dominant) | CH mmaps the memory-ranges file and fault-pages the guest RAM back in. On a GCE n2-standard-4 with a VM configured at 1–4 GB RAM this is the single largest physical step. Socket poll (step 5) blocks until CH binds `--api-socket`. |
| **`ch-remote resume`** | driver/CH | ~50–500 ms | HTTP RPC to CH's API socket. Brings vCPUs from PAUSED → RUNNING. Fast once socket is up. |
| **`wait_for_livez_blocking`** | controller (spawn_blocking) | ~500 ms–3 s | Polls `GET /livez` every 150 ms until 200. The agent is already in memory (restored from snapshot) — it answers quickly once the tap is UP and ARP resolves. Main latency source: tap UP propagation after resume (virtio-net re-attaches) + first ARP round-trip. |
| **`clock_resync_post_restore`** | controller (spawn_blocking) | ~50–200 ms | One signed POST to `/_clock_resync`. Single HTTP RTT. |
| **`register_restored` + CAS to `running`** | controller | ~50–200 ms | `NomadCHBackend::state` write (RwLock) + 1 pg write. |
| **Total measured p50** | — | **~50.7 s** | Dominant contributor is CH memory deserialisation (~10–40 s) + Nomad scheduling (~2–5 s). |

### Phase split estimate (p50 ~50.7 s)

| Sub-phase | Estimated contribution | Confidence |
|---|---|---|
| DB/pg overhead | ~0.3 s | High — loopback pg, 3 queries |
| store.get (SHA-256 verify + hardlink) | ~1–2 s | Medium — streaming 1 GB at 1 GiB/s NVMe; SHA-256 is the bottleneck |
| submit_restore_job POST + poll-running | ~2–4 s | High — Nomad scheduling is fast; driver StartTask is the slow part |
| startTaskRestoreBranch (pre-spawn setup) | ~0.2–0.5 s | High — FICLONE reflink is O(1), tap setup is fast |
| CH --restore (mmap + page fault) = API socket wait | **~35–43 s** | Medium-high — this is where ~85% of wake latency lives, consistent with "restoring ~40s" from harness JSON |
| ch-remote resume | ~0.1–0.5 s | High |
| wait_for_livez (tap UP + ARP) | ~0.5–2 s | Medium |
| clock_resync + register + CAS | ~0.3 s | High |

**Key finding**: the CH memory deserialisation + page-fault step (from CH spawn to API socket bind) accounts for ~35–43 s of the ~50 s p50. Everything else totals ~7–15 s.

---

## 2. Top 3 Latency Reduction Opportunities

### Opportunity 1 — Reduce `wait_for_agent_livez` poll cadence: 150 ms → 50 ms

**File**: `crates/sandbox/src/backend/nomad_ch.rs`, line **2965**  
**Current code** (line 2965): `compio::time::sleep(Duration::from_millis(150)).await;`  
**Also**: `wait_for_livez_blocking` in `crates/sandbox/src/restore_handler.rs`, line **1479**: `std::thread::sleep(Duration::from_millis(150));`

**Phase contribution**: `wait_for_livez_blocking` is expected to observe the first 200 within 1–3 polls after resume. At 150 ms cadence, 2 misses before a hit = 300 ms of dead sleep. At 50 ms cadence, that same 2-miss window = 100 ms.

**Fix**: Change both sleeps from 150 ms to 50 ms.

```rust
// nomad_ch.rs line 2965:
compio::time::sleep(Duration::from_millis(50)).await;

// restore_handler.rs line 1479:
std::thread::sleep(Duration::from_millis(50));
```

**LOC**: 2 lines changed.

**Risk**: Low. The /livez endpoint is a trivial `200 {"status":"ok"}` — there is no agent-side work gating the response. Polling 3× faster under c=1 adds negligible load on the agent or the host. Under high concurrency (c=20) the increased polling rate is still ~1 HTTP request per 50 ms per sandbox — well within the agent's capacity.

**Estimated p50 reduction**: 0–200 ms. The per-poll savings are small, but they compound with the `wait_for_alloc_running_blocking` cadence (see below). On cases where livez is discovered on the 2nd or 3rd poll, saves 100–200 ms directly.

---

### Opportunity 2 — Reduce `wait_for_alloc_running_blocking` poll cadence: 250 ms → 100 ms

**File**: `crates/sandbox/src/restore_handler.rs`, line **1446**  
**Current code**: `std::thread::sleep(Duration::from_millis(250));`  
**Also**: `wait_for_alloc_running` (async version) in `crates/sandbox/src/backend/nomad_ch.rs`, line **2481**: `compio::time::sleep(Duration::from_millis(250)).await;`

**Phase contribution**: Nomad's `ClientStatus` field transitions to `"running"` once the driver's `StartTask` returns. From the cutover data, `startTaskRestoreBranch` completes in ~3–20 s (dominated by the CH API socket poll). The controller polls Nomad at 250 ms cadence — the first poll after the transition incurs 0–250 ms of avoidable delay. Reducing to 100 ms saves 0–150 ms per wake.

**Fix**: Change both sleep values from 250 ms to 100 ms.

```rust
// restore_handler.rs line 1446:
std::thread::sleep(Duration::from_millis(100));

// nomad_ch.rs line 2481:
compio::time::sleep(Duration::from_millis(100)).await;
```

**LOC**: 2 lines changed.

**Risk**: Low-Medium. Nomad's `/v1/job/<id>/allocations` endpoint is a cheap pg-backed read. Under c=1 this doubles the Nomad poll rate for a single job over a 2–5 s window. Under c=20 it triples it — still trivially within Nomad's capacity on a dedicated server. One concern: if Nomad is on a shared host under heavy load, additional polling could amplify scheduling jitter. Not a concern on the GCE cluster topology (dedicated Nomad servers).

**Estimated p50 reduction**: 50–150 ms. Marginal in isolation but stacks with the livez cadence reduction.

---

### Opportunity 3 — Skip SHA-256 re-verification on `store.get` for the wake path (or make it async-parallel with Nomad job submission)

**File**: `crates/sandbox/src/snapshot_store.rs`, line **277**: `let (actual, _) = compute_artifact_sha256(&src)?;`  
Called from `do_restore_inner` in `crates/sandbox/src/restore_handler.rs`, lines **397–411** (spawn_blocking).

**Phase contribution**: `compute_artifact_sha256` streams all three artifact files through SHA-256. For a typical sandbox with 1 GB memory-ranges + small state.json + config.json, at ~500 MB/s disk read throughput this is ~2 s. At GCE NVMe speeds it may be closer to 1 s, but it is non-trivial and fully sequential with the rest of the wake path.

**The insight**: the artifact's integrity was already verified at `store.put` time (SHA-256 was computed then and stored in pg). The `expected_sha256` passed to `store.get` IS that previously-verified value. The wake path re-verifies the artifact on EVERY wake — including the happy path where nothing has changed. The only scenario where this re-verification provides value is bit-rot between the `put` and the `get`. This is a real concern for long-term storage, but for a fresh snapshot (just taken before the wake), the re-read adds ~1–2 s of wall time with negligible incremental safety benefit on that timescale.

**Three sub-options** (in ascending complexity):

**3a. Skip re-verify on `get`, rely on the existing `put`-time hash** (LOC: ~5 lines — remove the `compute_artifact_sha256` call in `LocalDiskSnapshotStore::get` and the GCS counterpart):
```rust
// snapshot_store.rs — remove lines 277-283 (the verify-before-restore block)
// The target dir fill (hardlinks) proceeds immediately.
```
Risk: Medium. Removes the defense against bit-rot between snapshot and wake. Acceptable for the current production posture (snapshots are short-lived, GCS has its own CRC32c/MD5 layer for the GCS backend). The `SnapshottedSuspect` state + periodic sweep provide the safety net.

**3b. Parallelize `store.get` with Nomad job submission** (LOC: ~30 lines — restructure `do_restore_inner` to spawn both futures concurrently; `store.get` fills the staging dir while Nomad's `submit_restore_job` POST + alloc_running poll runs in parallel).

This would overlap the ~1–2 s SHA-256 verify + hardlink with the ~2–5 s Nomad scheduling window, saving ~1–2 s with no correctness tradeoff (the driver doesn't READ the staging dir until after `alloc_running`; the staging dir must be populated before the driver starts, but Nomad job submission and alloc scheduling do not touch the dir).

Currently the code is fully sequential: `store.get` → `submit_restore_job` → `wait_for_livez`. Parallelizing `store.get` ∥ `submit_restore_job` removes ~1–2 s from the critical path.

**3c. Provide a `get_no_verify` variant** (LOC: ~15 lines) for callers with a verified-recently token.

**Recommendation for this investigation**: option 3b is the highest-leverage zero-safety-regression path — it parallelizes two independent operations and saves 1–2 s.

**LOC for 3b**: ~30 lines in `restore_handler.rs` (restructure `do_restore_inner` to use `tokio::join!` or compio equivalents). Since this is a compio/io_uring codebase, the pattern is already established: `spawn_blocking` for each synchronous op, then `.await` both futures.

**Risk for 3b**: Medium. The two operations are logically independent (store fills the staging dir; Nomad submits the job spec). Correctness requires that `wait_for_livez` is called AFTER both complete. The error handling on both branches must roll back properly (teardown_restore is already called on failure). A test covering "store.get fails, Nomad job submitted" must assert the teardown fires.

**Estimated p50 reduction for 3b**: ~1–2 s (overlaps ~half of the store.get wall time with Nomad scheduling).

---

## 3. Quick-Win Recommendation

**Ship next: Opportunity 1 + Opportunity 2 together (4 lines, ~20 min)**

### Exact changes

**File 1**: `crates/sandbox/src/backend/nomad_ch.rs`

Line 2481 (async `wait_for_alloc_running` poll sleep):
```rust
// Before:
compio::time::sleep(Duration::from_millis(250)).await;
// After:
compio::time::sleep(Duration::from_millis(100)).await;
```

Line 2965 (async `wait_for_agent_livez` poll sleep):
```rust
// Before:
compio::time::sleep(Duration::from_millis(150)).await;
// After:
compio::time::sleep(Duration::from_millis(50)).await;
```

**File 2**: `crates/sandbox/src/restore_handler.rs`

Line 1446 (blocking `wait_for_alloc_running_blocking` poll sleep):
```rust
// Before:
std::thread::sleep(Duration::from_millis(250));
// After:
std::thread::sleep(Duration::from_millis(100));
```

Line 1479 (blocking `wait_for_livez_blocking` poll sleep):
```rust
// Before:
std::thread::sleep(Duration::from_millis(150));
// After:
std::thread::sleep(Duration::from_millis(50));
```

### Expected p50/p99 reduction

Baseline wake p50: ~50.7 s.

- Cadence reduction saves **50–350 ms** in the best case (2–3 polls avoided across both phases).
- This is a **small but free** improvement with no risk. The dominant ~35–43 s phase (CH memory deserialisation) is not addressed by these changes.

**Honest assessment**: these four lines will not move the p50 below 50 s. They are a minor polish pass. The real lever for meaningful improvement (10+ s reduction) is Opportunity 3b (parallelise store.get ∥ Nomad submission, ~1–2 s) combined with addressing the fundamental CH restore time (~35–43 s), which requires either:

- **Smaller memory footprint at snapshot time** (snapshot a VM with less RAM allocated — current production likely snapshots at the full configured VM size, e.g. 1–4 GB; halving the RAM footprint would halve CH restore time proportionally).
- **Pre-warming / pre-fetching** the memory-ranges artifact to page cache before the Nomad alloc starts (the controller could `mmap` + `madvise(MADV_WILLNEED)` the memory-ranges file in the staging dir while Nomad scheduling is in flight).
- **Faster host I/O** — using `O_DIRECT` + async io_uring for CH's memory restore path (CH-internal change, out of scope for the controller/driver).

### Unit test to add

In `crates/sandbox/src/backend/nomad_ch.rs`, extend the existing `wait_for_agent_livez_times_out_on_fp_mismatch` test to assert that the function returns within `2 × new_cadence + 2 × request_timeout` of the deadline, proving the sleep value is honoured:

```rust
// Existing: wait_for_agent_livez_times_out_when_unreachable
// Extend: assert elapsed < timeout + 2 * Duration::from_millis(50)
//         (with the new 50ms cadence)
```

This test already exists in skeleton form at line 4157 (`wait_for_agent_livez_times_out_when_unreachable`); updating the comment + assertion bound to `50ms` instead of `150ms` documents the new cadence contract.

---

## Appendix: Constants Summary

| Constant | Location | Current Value | Notes |
|---|---|---|---|
| `wait_for_agent_livez` async poll cadence | `nomad_ch.rs:2965` | 150 ms | Hot path for create; also used in restore context |
| `wait_for_livez_blocking` poll cadence | `restore_handler.rs:1479` | 150 ms | Used by RealRestoreBackend on wake path |
| `wait_for_alloc_running` async poll cadence | `nomad_ch.rs:2481` | 250 ms | Cold-boot create path |
| `wait_for_alloc_running_blocking` poll cadence | `restore_handler.rs:1446` | 250 ms | Wake path Nomad poll |
| `defaultAPISocketPollTimeout` (driver) | `restore_task.go:288` | 60 s | CH socket readiness budget (widened from 10 s) |
| `defaultAPISocketPollInterval` (driver) | `restore_task.go:294` | 100 ms | CH socket poll cadence |
| `defaultAPISocketPollPerAttempt` (driver) | `restore_task.go:301` | 200 ms | Per-Dial timeout |
| `wakeRootfsLockWaitAttempts` (driver) | `restore_task.go:337` | 50 | OFD lock probe: 50 × 100 ms = 5 s budget |
| `wakeRootfsLockWaitInterval` (driver) | `restore_task.go:338` | 100 ms | OFD lock poll cadence |
| `alloc_running_timeout_secs` (controller config) | `nomad_ch.rs:751` | from config | Nomad scheduling deadline |
| `agent_livez_timeout_secs` (controller config) | `nomad_ch.rs:801` | from config | Total livez budget |
