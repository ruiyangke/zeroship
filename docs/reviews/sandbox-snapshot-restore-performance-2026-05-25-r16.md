# Sandbox/snapshot-restore — performance r16 review

Date: 2026-05-25 (UTC)
HEAD at audit: `792a7aa5`. Perf lens last reviewed at r15 (`2afbb2dd`);
all other lenses at r16. This round catches the perf lens up.

Smoke-r11 baseline (`...cluster-2026-05-25-T8b-smoke-r11.md`):
- CREATE: 6,424 ms · SNAPSHOT: 6,314 ms · WAKE: 50,083 ms (retry
  exhausted; teardown 60.166 s).

C-7-LT-PR1 (wake_jobs + WakeJobRow + detach_isolated) is in flight and
is the structural fix for WAKE. This review is the perf roadmap that
lands ON TOP of PR1.

## Summary

**8 new findings**, all OPEN, none CRITICAL. Top SLO-visible win is
**R16-P5** (gzip memory-ranges; ~3.5 s combined SNAPSHOT + WAKE).
**R16-P3** (fused AEAD-encrypt + canonical-SHA) saves ~1.5 s/SNAPSHOT.
**R16-P2** (livez transport timeout 500 → 200 ms) saves ~900 ms/WAKE.

Carry-forward from r15 unchanged: R9-P1, R10-P1/3/4/5/6/7, R11-P1/P4,
R9-#6/#8, R14-P1/P3, R15-P1.

## CRITICAL

None.

## IMPORTANT

### R16-P1 — wait_for_alloc_running 250 ms poll cadence too slow

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:2668`,
`crates/sandbox/src/restore_handler.rs:2399`.

Both variants sleep **250 ms** between Nomad `/v1/job/<id>/
allocations` polls. Nomad transitions `pending → running` in
~150–300 ms on a healthy fleet, so the controller spends on average
**~125 ms** between transition and noticing. Dropping to **100 ms**
saves ~75 ms per CREATE/WAKE alloc-wait. HTTP overhead at 4× cadence
is <4% of one core (local Nomad agent, unauthenticated GET).

**Risk**: ~zero. 100 ms is the cadence the rest of `nomad_ch.rs` uses
for the host-fence (per the comment at line 3259).

### R16-P2 — wait_for_livez transport timeout 500 ms is the floor

**File:Line:** `crates/sandbox/src/restore_handler.rs:2427-2432`,
`crates/sandbox/src/backend/nomad_ch.rs:3091`.

Both call `ureq::get(...).timeout(Duration::from_millis(500))`. On a
fresh VM, the first polls land while the agent isn't listening — the
500 ms TCP-connect timeout is wasted before the next 150 ms sleep, so
the **effective cadence is 650 ms** during the agent-boot window (~3
wasted polls × ~300 ms savings each).

Dropping to **200 ms** transport timeout (still 4× typical agent
response on loopback) saves **~900 ms/WAKE** and ~600 ms/CREATE.

**Risk**: low. Loopback `/livez` connect is sub-millisecond when
the listener is up; the 500 ms was sized for an external endpoint.

### R16-P3 — Fuse AEAD-encrypt + canonical-SHA into one read pass

**File:Line:** `crates/sandbox/src/snapshot_aead.rs:413-453` (encrypt),
`crates/sandbox/src/snapshot_store.rs:184-223` (canonical SHA, called
at line 235).

Snapshot critical path is: AEAD encrypt (read 1 GB plaintext + write
1 GB ciphertext + rename) → L1.put canonical SHA (read 1 GB
ciphertext) → L1 rename → return. The L2 GCS upload at
`snapshot_store_gcs.rs:1139` is fire-and-forget (`std::thread::spawn`)
so doesn't gate RPC return.

Smoke-r11 SNAPSHOT 6.3 s breakdown: ch-remote ~2.1 s + ~4.2 s for
2× read + 1× write on the staged 1 GB at ~500 MB/s SSD.

`encrypt_in_place` already streams plaintext through ChaCha20-Poly1305;
feeding each ciphertext chunk into a `Sha256::update` as it's written
fuses passes 1 and 2 (pass 1's write IS pass 2's input source).
**Saves ~1.0–2.0 s/SNAPSHOT** (page-cache-dependent).

**Risk**: medium. Plumbs `Sha256` through `encrypt_in_place`; existing
canonical-SHA tests (`snapshot_aead.rs:875-1008`) regression-pin the
hash domain.

### R16-P4 — L2 background thread reads memory-ranges 3× (off-path)

**File:Line:** `crates/sandbox/src/snapshot_store_gcs.rs:565-648`
(`GcsSnapshotStore::put`).

`canonical_artifact_sha256` (1 GB read) + `sha256_file` (1 GB read) +
`upload_resumable` chunk loop (1 GB read) = **3 × 1 GB** re-read from
L1 per L2 upload. Off SNAPSHOT critical path, but under c=20 produces
60 GB of background read traffic competing with foreground disk-image
fmkfs.ext4 and ch-remote dumps.

Fix: compute both SHAs during the upload chunk loop (one read).
Saves 2 × 1 GB SSD reads per L2 upload.

**Risk**: low. Hash is a pure function of file bytes.

### R16-P5 — gzip memory-ranges before AEAD: 1 GB → ~50 MB

**File:Line:** `crates/sandbox/src/snapshot_aead.rs:411` (chunk loop),
`crates/sandbox/src/snapshot_store_gcs.rs:354-411` (upload).

A fresh CH guest's 1 GB memory-ranges file is ~95% zero pages
(idle Debian rootfs post-init). gzip level 1: ~30–50 MB output;
~600–900 MB/s on one core for mostly-zero input.

Apply BEFORE AEAD (ciphertext is incompressible). Natural place: fold
gzip into `encrypt_in_place`'s chunk loop — read plaintext → gzip-
encode → AEAD-encrypt the gzipped chunk → write. One byte in the
AEAD header's reserved word at `snapshot_aead.rs:406` flags
compression.

**Combined wins**:
- SNAPSHOT: AEAD work drops 20× → **~1.0–1.5 s saved**.
- WAKE (L1 miss): GCS download 1 GB → 50 MB → **~1.5–2.0 s saved**.
- L1 disk: ~95% saved → L1 cache holds ~20× more snapshots.
- L2 GCS storage: ~95% saved.

Total SLO-path win: **~3.5 s combined** (SNAPSHOT 1.5 s + WAKE 2 s).

**Risk**: medium. AEAD header version bump; decrypt_to learns to
gunzip. Security lens should sign off on the gzip-before-encrypt
ordering (preserves AEAD IND-CCA).

### R16-P6 — GCS resumable chunk 8 MiB too small for 1 Gbps egress

**File:Line:** `crates/sandbox/src/snapshot_store_gcs.rs:69`.

`RESUMABLE_CHUNK_SIZE = 8 MiB` → 128 chunks for 1 GB memory-ranges.
Each chunk = serialized PUT + 308-parse RTT (~30–60 ms on healthy
GCS). 128 × 40 ms ≈ **5.1 s of serialized RTT** vs ~8 s payload at
1 Gbps → ~40% of L2 upload wall is RTT.

Bumping to **32 MiB** → 32 chunks × 40 ms ≈ 1.3 s RTT, **saves
~3.8 s** on L2 upload background. Becomes irrelevant if R16-P5 lands
first (post-gzip artifact fits in 2 chunks).

Controller RAM cost: one 32 MiB `vec![]` × concurrent uploads;
trivial on a 64 GB host. GCS allows up to 5 GB per chunk.

**Risk**: ~zero. Tuning knob.

### R16-P7 — Tiered::put thread-spawn + allocs per snapshot

**File:Line:** `crates/sandbox/src/snapshot_store_gcs.rs:1117-1138`.

Per snapshot: PathBuf clone (1117), 2 String clones (1112, 1118),
`format!` for thread name (1138), `std::thread::Builder::spawn`.
Sub-ms overhead; **MINOR**. R14-P3's "unbounded thread spawn"
unchanged — bounded under PR1's wake_jobs concurrency cap.

**Risk**: INFO at current load.

### R16-P8 — L1 cache hit/miss instrumentation absent

**File:Line:** `crates/sandbox/src/snapshot_store_gcs.rs:1173-1201`
(`Tiered::get`).

L1 hit vs L1-miss-fell-through-to-L2 emits no metric or
distinguishing log. **Blocks Tier-4 pre-warm work** — can't
prioritize pre-warm without a hit-rate measurement (per the user's
"never estimate without a measurement" rule).

Minimum viable: a `metric_l1_hit_inc()` / `metric_l1_miss_inc()` pair
on the two branches at line 1180-1192.

**Risk**: ~zero.

## MINOR

- **R16-M1** `download_to_disk` re-acquires token lock per file
  (`snapshot_store_gcs.rs:427`). ~50 ns/call; not material.
- **R16-M2** `urlencoding` allocates fresh String per HTTP call
  (`snapshot_store_gcs.rs:551-562`). ~6 µs cumulative/snapshot.

## Quantified-win roadmap

| Finding | File:Line | Win | Effort | Risk |
|---|---|---|---|---|
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms (CREATE + WAKE) | 2 LOC | ~zero |
| **R16-P2** livez timeout 500 → 200 ms | `nomad_ch.rs:3091`, `restore_handler.rs:2427` | ~900 ms WAKE, ~600 ms CREATE | 2 LOC | low |
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | ~1.0–2.0 s/SNAPSHOT | ~80 LOC + tests | medium |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT 1.5 s + WAKE 2 s + 95% storage | ~200 LOC + format-rev test | medium |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s off L2 background | 1 LOC | ~zero |
| **R16-P7** Tiered::put alloc reduction | `snapshot_store_gcs.rs:1117-1138` | sub-ms | ~20 LOC | low |
| **R16-P8** L1 hit/miss metrics | `snapshot_store_gcs.rs:1173-1201` | enables Tier-4 pre-warm | ~10 LOC | ~zero |

**Top-3 SLO-visible wins:**
1. **R16-P5 gzip** — ~3.5 s SNAPSHOT+WAKE + 95% storage.
2. **R16-P3 fused AEAD+SHA** — ~1.5 s/SNAPSHOT RPC critical path.
3. **R16-P2 livez timeout** — ~900 ms/WAKE off agent-boot wait.

## Cross-lens consensus

- **Architecture r16**: C-7-LT-PR1 wake_jobs is the structural WAKE
  fix; perf r16 lands on top, no competition.
- **Code quality r16**: R14-P3 (std::thread::spawn) — carried as
  R16-P7 MINOR.
- **Security r16**: R16-P5's gzip-before-encrypt ordering MUST be
  reviewed — preserves AEAD IND-CCA but the header version-rev needs
  the security lens sign-off.
- **API surface r16**: no perf cross-cuts.

## Lens hand-off

**Tier 2 (1 PR, low risk, ~1.6 s SLO win):**
- R16-P1 + R16-P2 (cadence + timeout). Bundle as one diff.

**Tier 3 (2 PR, medium risk, ~5 s SLO win):**
- R16-P3 fused encrypt + SHA (PR-a).
- R16-P5 gzip-before-encrypt (PR-b, depends on PR-a's chunk-loop
  refactor). R16-P6 drops out after R16-P5.

**Tier 4 (1 PR, low risk, instrumentation):**
- R16-P8 L1 metrics. Block pre-warm work until we have a hit-rate
  measurement.

R16-P4 + R16-P7 are off-critical-path; defer until SLO items land.

## Focus-area notes (brief's questions)

**(1) /livez cadence**: the sleep cadence (150 ms) isn't the floor —
the **500 ms transport timeout** during the dead-TCP window is.
R16-P2 captures it.

**(2) Snapshot upload pipe**: AEAD seal and GCS upload are **already
decoupled** — L2 is fire-and-forget at `snapshot_store_gcs.rs:1139`.
The synchronous gate is AEAD-encrypt + L1 canonical-SHA (R16-P3),
not GCS. Stream-seal → stream-upload doesn't help because L1 must
land before snapshot RPC returns (durability contract at
`snapshot_handler.rs:381-407`). memory-ranges is **never fully
allocated in RAM** — read in 64 KiB / 8 MiB chunks throughout.

**(3) GCS roundtrips**: snapshot upload = 1 resumable-init POST +
~128 chunk PUTs + 2 single-shot PUTs = ~131 calls (token cached, so
~0 amortized). Wake download = 3 GETs. R16-P6 cuts upload to ~35
calls; R16-P5 collapses memory-ranges to a single-shot PUT (3 total).
No content-addressed elision opportunity (object names already
encode `sandbox_id`).

**(4) CREATE 6.4 s breakdown**: Nomad alloc scheduling ~3.5 s
(operator-controlled), wrapper bash ~0.5 s (closing with Go-driver
cutover), agent boot + /livez wait ~1.5 s (R16-P2 saves ~600 ms),
disk-image materialization ~0.3 s (idempotent), HTTP submit + poll
cadence ~0.6 s (R16-P1 ~75 ms).

**(5) WAKE wall-time post-PR1** (no deadline cap):
- `submit_restore_job` ~2.5 s · `wait_for_livez` ~2.0 s · `store.
  get` AEAD-disabled L1-hit ~1.0 s · AEAD-active L1-hit ~1.5–2.0 s
  (R9-P1 still open) · `clock_resync` ~0.1 s · `register_restored`
  <0.01 s · L1-miss adds ~1.5–2.0 s GCS download.
- Projected: **p50 ~5.6 s** (AEAD-disabled, L1-hit). **p99 ~8.5 s**
  (AEAD-active, L1-miss). With R16-P3 + R16-P5: **p50 ~3.5 s, p99
  ~6.0 s**.
- Dominant cost is `submit_restore_job` + `wait_for_livez`, both at
  Nomad/CH floors.

**(6) Large allocations**: **none today**. AEAD uses 8 MiB chunks
(`snapshot_aead.rs:411`), SHA reads 64 KiB (`snapshot_store.rs:207`),
GCS uploads 8 MiB chunks. C-8b's retry loop allocates one `Instant`
+ small Strings per attempt — sub-ms. No new large allocations.

## Carry-forward (from r15)

| Finding | Status | File:line |
|---|---|---|
| **R11-P1** Fresh pg Pool/method | OPEN | `db.rs:492-516` |
| **R10-P1** AEAD 5-pass memory-ranges reads | OPEN (R16-P3 + R16-P4 are the surgical halves) | `snapshot_aead.rs:377`, `snapshot_store.rs:184`, `snapshot_store_gcs.rs:540` |
| **R10-P3** cipher.encrypt allocs Vec/chunk | OPEN | `snapshot_aead.rs:411-446, 529-572` |
| **R10-P4** GCS recomputes canonical SHA | OPEN (R16-P4 closes via L2 fuse) | `snapshot_store_gcs.rs:559-560, 1066` |
| **R10-P5** AEAD raw File (no BufWriter) | OPEN | `snapshot_aead.rs:403, 529` |
| **R10-P6** ~32 fresh ureq conns/wake | OPEN | `restore_handler.rs:1465-1490` |
| **R10-P7** `clock_resync_random_hex` format!-loop | OPEN | `restore_handler.rs:1687-1772` |
| **R9-P1** AEAD wake discards hard-link | OPEN | `snapshot_aead.rs:633-678` |
| **R9-#6** 64 KiB scratch in artifact loop | OPEN | `snapshot_store.rs:207`, `snapshot_store_gcs.rs:534, 1011` |
| **R9-#8** chunk_aad 13-byte Vec/chunk | OPEN | `snapshot_aead.rs:325-330` |
| **R11-P4** Sweep SandboxRow::clone | OPEN | `sweep.rs:494-526` |
| **R14-P1** C-4 retry-tail | OPEN (PR1 removes deadline) | `restore_handler.rs:265-306` |
| **R14-P3** std::thread::spawn unbounded | INFO (= R16-P7) | `snapshot_store_gcs.rs:1132` |
| **R15-P1** C-8 30 s fence vs Nomad-purge tail | INFO | `gcp-worker-startup.sh:455-470` |

## Closed since r15: none.

## Ranked next-biggest perf lever (updated)

1. R16-P5 gzip — ~3.5 s SLO + 95% storage.
2. R16-P3 fused encrypt + SHA — ~1.5 s/SNAPSHOT.
3. R16-P2 livez timeout — ~900 ms/WAKE.
4. R11-P1 per-thread pg-pool — robustness + ~40 ms/wake.
5. R9-P1 AEAD hard-link discard — ~0.5–1.5 s/AEAD wake.
6. R16-P1 alloc cadence — ~150 ms CREATE+WAKE.
7. R16-P6 chunk size — moot after R16-P5.
8. R10-P6 cached ureq::Agent — ~30–100 ms/wake.
