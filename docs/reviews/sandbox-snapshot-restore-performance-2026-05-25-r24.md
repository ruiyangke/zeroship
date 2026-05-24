# Sandbox/snapshot-restore — performance r24 review

Date: 2026-05-25 (UTC).
HEAD at audit: `a482f00d` (branch `master`, worktree
`sandbox-snapshot-restore`).
Prior: `…performance-2026-05-25-r23.md` (last performance round).
Catch-up: r24 picks up after the v33/v34 controller landings,
T-8b-stress-r2 RED, harness-in-repo (R24-T1) closure, and the
R23-API1/R25-S1/R25-I1/R25-I2 typed-error work that pushed the
preflight failure mode through the wire as `staging_image_missing`.

Round 24 evaluates **five new landings since r23**:

1. `d638b10f` — `nomad-ch` leaks `host_dir` on stop + verbatim
   driver-msg propagation (v34 controller-side half of the
   T-8b-stress-r2 fix).
2. `e82bffd7` — `sandbox/sweep` `spawn_host_dir_gc` task added
   (v34 sweeper-owned host_dir GC; ~280 LOC of new sweep code in
   `crates/sandbox/src/sweep.rs:867-1190`).
3. `c729c2b8` — driver bump v13 → v14 + controller v33 → v34
   (scripts-only; out of scope per directive).
4. `79871194` + `022f778a` — `WakeErrorCode::StagingPathMissing`
   variant + `SubmitRestoreError` typed enum on the
   `submit_restore_job` boundary (R23-API1, R25-S1, R25-I1, R25-I2).
5. `dd2079a9` + `61492e54` — in-repo harness (`snapshot_stress.py`)
   + GCP worker startup SHA-pin (R24-T1; one-shot boot verification).

## Summary

**6 findings, 0 CRITICAL, 1 IMPORTANT, 5 MINOR.** R23-A2
(try_create blocking ntex worker) **confirmed open at HEAD**;
priority elevated below. R23-P1 (R11-P1 pool churn) still open
with no new measurement. R24's primary new exposure is the
sweeper-owned host_dir GC and the host_dir leak it relies on:
both are correct by construction, both shift the steady-state
storage footprint, neither changes the wake/snapshot wall.

The R24 landings move the perf surface in two directions:

- **Up**: the host_dir leak grows linear-with-creation-rate
  storage (workspace.img ~512 MB + tail). At 1 sandbox/min creation
  with the 1-hour grace = ~60 stranded dirs ≈ **~30 GB peak
  stranded** before the sweep reaps. This is bounded but real;
  see R24-M2 for the math.
- **Down**: the host_dir GC sweeper is **the only new pg cost** on
  the periodic loop axis; it issues `O(N_subdirs × 2)` SELECTs per
  5-minute tick. At realistic fleet sizes the cost is dominated by
  the `rm -rf` wall, not pg, and the pg work hits indexed
  partitions on both lookups. **Negligible at fleets <1000;
  monitor at fleets >10k**. See R24-M1.

The typed `SubmitRestoreError` + `WakeErrorCode::StagingPathMissing`
landings have **zero measurable per-call cost** (R24-M3).

The wake_jobs wire envelope for a typical `StagingPathMissing`
poll response is **~280 bytes** (down from the round-22 hand-wave
of "~1 KB"); see R24-M4. Not a perf concern.

**No measurement-changing landings since r23.** SNAPSHOT p50
remains at the measured 14.6 s; WAKE p50 at 46.9 s. Phase-3 (R16-P3
+ R16-P5) and R11-P1 perf carries unchanged.

## CRITICAL

None. The host_dir GC is the right architectural shape for the
race it closes (the per-alloc rm race against retry-CREATE
StartTask); the cost it adds is bounded and not on a hot path.

## IMPORTANT

### R24-P1 — host_dir leak peak storage at sustained create rate: **30–60 GB stranded steady state** at modest fleet shapes

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:85-128`
(lifecycle doc + intentional-leak contract) + `crates/sandbox/src/
sweep.rs:830-1190` (sweeper) + `crates/sandbox/src/sweep.rs:886`
(`HOST_DIR_GC_GRACE_SECS = 3600`).

**The math.** Steady-state stranded count is bounded by:

```
stranded_peak = (create_rate_per_sec × grace_secs)  +  (sweep_lag)
              = (X / 60)         × 3600             +   ~300 s
              = 60 × X           +   ~5 × X         per minute
              ≈ 65 × create_rate_per_min
```

Per-dir size (from `crates/sandbox/src/backend/nomad_ch.rs:721-726`
+ `create_ext4_image_if_missing` at `:3656`):
- `workspace.img`: `workspace_image_size_gb` (default 20 GB, but
  sparse — actual on-disk is post-mkfs.ext4 metadata + whatever the
  guest wrote; ~50-500 MB after a typical sandbox session, depending
  on user activity).
- `restore/` (post-WAKE): memory-ranges, ~512 MB to ~2 GB depending
  on guest RAM and decompression staging.
- `meta/` + sealed-auth records: <1 MB.
- **`home.img` is per-USER not per-sandbox** (lives at
  `<user_home_dir_root>/<user>/home.img`, NOT under
  `<host_dir>/<uuid>/`), and is intentionally skipped by the
  sweeper (`crates/sandbox/src/sweep.rs:969-975` `if name == "users"
  { continue; }`). So the per-stranded-host_dir size is the
  workspace.img + restore + meta, NOT 2× the disk image.

**At 1 create/min, sandbox session writes ~200 MB to workspace,
restore stages ~512 MB**:

| Creation rate | Stranded dirs (steady) | Per-dir bytes | Peak GB |
|---:|---:|---:|---:|
| 0.1 /min | 6 | ~750 MB | ~4.5 GB |
| 1 /min | 65 | ~750 MB | ~49 GB |
| 10 /min | 650 | ~750 MB | **~488 GB** |
| 100 /min | 6500 | ~750 MB | **~4.9 TB** |

The mandate's "1-hour grace" defaults are forensic-friendly but
**N2-standard-32 ships with 100 GB boot disk by default** and the
controller-side `/var/zeroship/ch/` is typically on local SSD with
a few hundred GB. At ≥10 creates/min sustained, the host disk fills
**before** the sweeper reaps, and the next `truncate -s 20G
workspace.img` either ENOSPC's the host or breaks the next CREATE.

**Why this matters NOW:** T-8b-stress projection is c=20 over 60
cycles in burst. That's ~12 creates/sec for the burst window. Even
amortized at ~2 creates/min sustained over a long run, this hits
the 10/min steady-state regime in a few minutes of heavy use.

**Why:** v34's host_dir leak contract is unbounded over an
unbounded creation rate. The 1-hour grace is a forensic budget; the
**volumetric** budget (host disk free space) is implicit and
unmeasured.

**Fix:** Three options, listed in increasing intrusiveness:

1. **Tighten `HOST_DIR_GC_GRACE_SECS` default to 600 s (10 min)**
   (`sweep.rs:886`). Forensic value declines sharply past ~10 min
   on a busy fleet; the operator can still set `=3600` via env.
   Cuts the stranded-peak factor by 6×. **Effort: 1 LOC; risk: ~zero
   (env-tunable already); priority: P2 NEW carry.**
2. **Add a host-disk-free-space watchdog** in `run_host_dir_gc_once`
   that overrides the grace when `statvfs(host_state_dir)` reports
   <10% free. Frees forensic dirs only under disk pressure;
   otherwise honors the grace. **Effort: ~40 LOC; risk: low.**
3. **Reap the bulk of the dir but keep `meta/` for forensics**.
   Splits the per-dir size cost from the forensic cost. **Effort:
   ~80 LOC; risk: medium.**

Recommend option 1 as a one-LOC follow-up; option 2 if cluster
operations confirms host-disk pressure during the post-cutover ops
window. **NEW carry: R24-A1 (host_dir GC grace tightening).**

## MINOR

### R24-M1 — host_dir GC sweep pg cost at scale: O(N_subdirs × 2 SELECTs) per 5-min tick; both indexed

**File:Line:** `crates/sandbox/src/sweep.rs:909-1131`
(`run_host_dir_gc_once`); query sites at `:1038`
(`db.get_sandbox_row(sandbox_uuid)`) and `:1078`
(`db.find_pending_wake_for_sandbox(...)`).

**Per-tick pg work** (when the dir survives the mtime grace +
parses as a Uuid):

1. `get_sandbox_row(sandbox_uuid)` — PK lookup on `sandboxes`
   table. Index: `sandboxes_pkey` (every pg-managed table has a PK
   index by default). **One index seek + heap fetch.**
2. `find_pending_wake_for_sandbox(typed_id)` — partial-index range
   probe. Index: `wake_jobs_sandbox_pending_uniq` (UNIQUE, partial
   `WHERE state NOT IN ('ok','failed')` — migration 0011 line 30).
   **One index seek with `LIMIT 1`**, falls back to
   `wake_jobs_sandbox_idx` (migration 0009 line 106) for the
   non-partial case but in practice the partial covers the hot
   query shape.

**Cost per dir** (local pg, ~1 ms RTT, hot cache):
- `get_sandbox_row`: ~1-2 ms (1 query roundtrip + 1 index fetch +
  1 heap fetch).
- `find_pending_wake_for_sandbox`: ~1-2 ms (1 query roundtrip + 1
  partial-index fetch + 1 heap fetch under LIMIT 1).
- Plus **`open_pool()` × 2** per dir (R11-P1 carry; same pool-churn
  hazard). At ~8 ms/pool that's another ~16 ms PER DIR PER TICK.

**At 1000 stranded dirs per tick** (10 creates/min × 1 hour grace,
worst case):
- Without R11-P1: 2 SELECTs × 1000 dirs × ~1.5 ms = **~3 s of pg
  wall**, PLUS 2000 pool opens × ~8 ms = **~16 s of TCP+SCRAM
  handshake** = **~19 s total**.
- With R11-P1: 2 SELECTs × 1000 × ~1.5 ms = **~3 s of pg wall** +
  cached pool overhead → **~3 s total**.

The sweep runs on `detach_isolated` (dedicated OS thread + private
compio runtime, `sweep.rs:1154`), so this **does not block ntex
worker handlers**. Even ~19 s of wall on a 5-min cadence is **<7%
duty cycle** on the dedicated thread, leaving headroom. At fleets
<1000 (the realistic short-term target) the wall is well under 5 s.

**Cross-lens carry:** R11-P1 elevation in r23 cited "c=20 stress
will mint 280 conns/s." The host_dir GC's per-tick conn churn
(2 × N_subdirs at default grace) **converges to the same hazard at
N_subdirs > ~250**: 500 pool-opens in one tick equals the same
new-connection rate as one c=20 wake burst. R11-P1 fix is now
**doubly motivated** — covers both the wake path AND the GC sweep
path. No new fix, just a sequencing reinforcement.

**Why:** the per-dir cost is small but the per-tick aggregate
scales linearly with stranded-dir count, and the stranded-dir
count scales linearly with `creation_rate × grace_secs` (R24-P1).

**Fix:** No code change. Two carry items:
1. R11-P1 priority sustained (already at top of Tier 2.75).
2. R24-A1 (NEW; grace shorter default) reduces N_subdirs and thus
   pool-churn proportionally.

Optional R24-A2 (NEW): **batch the per-dir pg lookups** into a
single `SELECT sandbox_id FROM sandboxes WHERE sandbox_id = ANY($1)
AND status IN ('stopped','lost','orphan')` + a single
`find_pending_wake_for_sandboxes_batch($1)`. Cuts the per-tick
roundtrips from `2N` to `2`. **Effort: ~80 LOC (one new db helper);
risk: medium**. Not justified at N<1000; flagged for the post-
cutover ops window. **NEW carry.**

### R24-M2 — host_dir leak per-dir size: workspace.img sparse; effective bytes ~50–500 MB, not the 512 MB carry assumption

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:3656`
(`create_ext4_image_if_missing`); per-dir layout per `:721-726`.

**Original carry estimate (r23/round-22):** "workspace.img ~512 MB
+ user_home.img ~512 MB + restore/ ~512 MB ≈ 1.5 GB/dir." **This
overstates by 2-3×**:

1. `home.img` is **NOT under the leaked host_dir** — it lives at
   `<user_home_dir_root>/<user_id>/home.img` per `nomad_ch.rs:586-
   589`, and the sweeper explicitly skips `"users"` at
   `sweep.rs:973`. Subtracting ~512 MB.
2. `workspace.img` is **sparse-truncated** to `workspace_image_size_gb`
   (default 20 GB) and `mkfs.ext4`'d. On-disk size after mkfs is
   ~12-20 MB (ext4 superblock + group descriptors + journal). The
   sandbox-active wall growth depends on guest activity; a no-op
   sandbox finishes at ~12-50 MB on disk, a heavy-use sandbox at
   ~200-500 MB.
3. `restore/` is materialised by `restore_handler.rs` only when the
   sandbox was actually WAKEd; CREATE-only stranded dirs never
   carry it. CREATE-fail strands (the v34 fix's primary case) thus
   carry ~50 MB max.

**Revised per-dir size:**
- CREATE-fail stranded dir (the v34 fix's primary case): ~12-50 MB.
- Stop-after-wake stranded dir (workspace populated by guest +
  restore/ artefacts): ~200-800 MB.
- **Realistic mix (80% CREATE-fail, 20% stop-after-wake):**
  ~50-200 MB/dir average.

**Re-running R24-P1's math with the corrected size:**

| Creation rate | Stranded dirs | Avg dir | Peak |
|---:|---:|---:|---:|
| 1 /min | 65 | 200 MB | ~13 GB |
| 10 /min | 650 | 200 MB | **~130 GB** |
| 100 /min | 6500 | 200 MB | **~1.3 TB** |

Better than r24-P1's original projection but still material at
sustained ≥10/min. R24-A1 (grace tightening) still applies; the
absolute numbers are 3× kinder than r24-P1 first claimed.

**Why:** carry estimates from prior rounds quoted
`workspace_image_size_gb` (the SPARSE allocation cap) as the actual
size; the on-disk size is dominated by actual writes, not the cap.

**Fix:** No code change. Update r24-P1's storage table to reflect
the corrected ~200 MB/dir average (see R24-P1's "stranded dirs ×
per-dir bytes = peak GB" — divide by ~3-4×).

### R24-M3 — typed `SubmitRestoreError` variant: zero hot-path allocation cost; pattern-match is unconditional jump

**File:Line:** `crates/sandbox/src/restore_handler.rs:158-225`
(`SubmitRestoreError` enum + `Display` + `log_detail`); call sites
at `:2257`, `:2264`, `:2284`, `:2288` (constructors); pattern-match
at `:1019` (`submit_restore_job` caller).

**Anatomy:**

```rust
// restore_handler.rs:158
#[derive(Debug)]
pub enum SubmitRestoreError {
    Preflight {
        which: &'static str,   // discriminant ptr (no alloc)
        path: PathBuf,         // already owned at call site
        source: String,        // already alloc'd by caller
    },
    Other(String),             // pre-existing shape
}
```

**Per-call cost on the happy (no preflight failure) path:** **zero**
— `submit_restore_job` returns `Ok(())` and the caller's `match`
arm at `restore_handler.rs:1018` (`Ok(()) => {}`) is one branch
taken. No enum construction, no allocation.

**Per-call cost on the failure path:** the `Preflight` variant
captures `(&'static str, PathBuf, String)` — the `PathBuf` and
`String` are already constructed at the call site (the
`workspace_img` arg passed by value; the source error message
already alloc'd by `assert_disk_image_present`). The enum
construction is a stack move into the discriminant + 3 fields. **No
Box, no Vec, no heap allocation introduced by the typed shape.**

**Pattern-match cost** at `submit_restore_job` caller
(`restore_handler.rs:1017-1035`):
- The arm tag check is a single discriminant compare (i64 cmp,
  ~1 ns).
- `Err(e @ SubmitRestoreError::Preflight { .. })` destructures into
  `which/path/source` via the `let SubmitRestoreError::Preflight {
  which, .. } = e else { unreachable!() }` at `:1025` — one
  unconditional re-binding; **the `unreachable!()` arm is provably
  unreachable** (the outer `match` already proved the discriminant).
  rustc + LLVM elide it; **zero cost in optimized builds**.

**`classify_failure` pattern-match overhead** (`wake_machine.rs:691`):
adds one new match arm in the failure classifier. The match itself
is **one branch in a jump table over ~10 arms**; one extra arm adds
**~zero ns** on the cold (failure) path. The hot (no-failure) path
never enters this function.

**Hot-path total cost added by R23-API1 / R25-S1 / R25-I1 / R25-I2:
0 ns.** Failure-path total cost added: **~1 ns** for the extra
arm tag + branch.

**Why:** typed-error shapes have negligible overhead in Rust when
the variant fields are already-owned values; the only real cost is
the `Display` impl's `write!` which only fires on the failure path.

**Fix:** No change. Carry-forward in the perf ledger as **closed
zero-cost**.

### R24-M4 — wake_jobs row wire envelope for StagingPathMissing: ~280 bytes; acceptable

**File:Line:** `crates/sandbox/src/admin_handlers.rs:1306-1314`
(sync-wake error response) + `crates/sandbox/src/db.rs:1432-1433`
(`error_code: Option<WakeErrorCode>` + `error_message:
Option<String>` in `WakeJobRow`); typed-code wire form at
`db.rs:1653` (`"staging_image_missing"`).

**Per-failure wake-poll response payload** (what the client sees
on `GET /admin/sandboxes/{id}/wake/{wake_id}` after a
`StagingPathMissing` failure):

```json
{
  "wake_id": "wak_01h00000000000000000000000",          // 32 chars
  "sandbox_id": "sbx_01h00000000000000000000000",       // 32 chars
  "state": "failed",                                     // 6 chars
  "error_code": "staging_image_missing",                 // 22 chars
  "error_message": "staging image missing: workspace.img for sbx_01h00000000000000000000000",  // ~70 chars
  "started_at_secs": 1730000000,                         // 10 chars
  "updated_at_secs": 1730000005,                         // 10 chars
  "ready_at_secs": null,
  "agent_url": null,
  "lessee": "ctl_01h00000000000000000000000",            // 32 chars
  "lessee_updated_at_secs": 1730000005                   // 10 chars
}
```

**Estimated JSON-encoded size:** ~280-320 bytes including key
labels, quotes, commas, and braces. (Versus a happy `state: "ok"`
response without error fields: ~180 bytes.) **Delta from
error_code/error_message addition: ~100 bytes/failed-wake
response.**

**At c=20 stress with 100% StagingPathMissing failures** (the
worst case during T-8b-stress-r2's repro): 20 polls/cycle × 60
cycles × ~300 bytes = ~360 KB of additional response traffic per
stress run. **Negligible.**

**At sustained 1 wake/sec with 10% failure rate** (a realistic
post-cutover production shape): 1/sec × 0.1 × 300 bytes = ~30
bytes/sec. **Trivial.**

**Pg storage cost per failed wake_jobs row:** ~100 additional bytes
(error_code: ~22 bytes + TOAST padding; error_message: ~70 bytes;
plus TIMESTAMPTZ for lessee_updated_at = 8 bytes already counted in
the base row). **Negligible against the row's existing ~200 bytes
of headers + state machine fields.**

**TOAST risk:** `error_message` is TEXT; pg toasts >2KB strings.
The longest realistic message is the path-free
`"staging image missing: user_home.img for sbx_..."` at ~70 bytes —
**three orders of magnitude below the TOAST threshold**. No row
fragmentation.

**Why:** error_code + error_message are short bounded strings; the
wire envelope is dominated by the per-row metadata, not the new
fields.

**Fix:** No change. Carry-forward as **closed**.

### R24-M5 — `try_create` blocking ntex worker confirmed open at HEAD; R23-A2 carry unchanged

**File:Line:** `crates/sandbox/src/handlers.rs:225` (handler call
site: `state.backend.create(sandbox_id, &user_id, &project_id)
.await`); `crates/sandbox/src/backend/nomad_ch.rs:489-622` (`create`
async fn); `:629-728` (`try_create` async fn body with sync
`std::fs::create_dir_all`, `create_ext4_image_if_missing`).

**Verbatim verification at HEAD `a482f00d`:**

```rust
// handlers.rs:224-225
let sandbox_id = Uuid::now_v7();
let res = state.backend.create(sandbox_id, &user_id, &project_id).await;
```

**Not wrapped in spawn_blocking.** The `try_create` body runs sync
syscalls on whatever runtime polls the handler future — ntex worker
in production. The sync operations performed inline:

1. `std::fs::create_dir_all(host_dir)` at `nomad_ch.rs:713` —
   syscall, ~50-500 µs.
2. `std::fs::create_dir_all(parent)` at `:716` — syscall, ~50-500
   µs.
3. `create_ext4_image_if_missing(&workspace_img, ...)` at `:723` —
   on first invocation per dir: `truncate -s 20G` subprocess
   (~5-20 ms) + `mkfs.ext4` subprocess (~1-3 s) + (v33 fix)
   `fsync_dir(parent)` (~1-5 ms). On warm reuse: `fs::metadata` +
   `assert_disk_image_present` stat-only path (~10-100 µs).
4. `create_ext4_image_if_missing(user_home_img, ...)` at `:725` —
   same shape; warm reuse common after first sandbox per user.

**Per cold-boot first-sandbox-per-user CREATE: ~3-5 s of sync
blocking on the ntex worker thread.** Per warm-home CREATE:
~1-3 s. At c=20 concurrent CREATE bursts the ntex worker pool
serialises behind these blocks; sibling requests (wake-POSTs,
status queries, admin polls) queue.

**Cross-lens carry confirmation:** R23-A2's hazard is unchanged at
HEAD. The v34 host_dir leak does NOT remove any sync work from
`try_create` (the leak only affects the cleanup path, not the
create path); the v33 `fsync_dir` adds ~1-5 ms sync work (already
flagged in r23 as M1, not a regression on top of mkfs.ext4).

**Why:** the `try_create` function's sync interior was never
migrated to `spawn_blocking`; the C-7-LT-style "wrap in
spawn_blocking on a dedicated runtime" pattern that the WAKE path
adopted has no twin on the CREATE path.

**Fix:** Carry R23-A2 forward unchanged. **~30 LOC: wrap the
`try_create` call in `compio::runtime::spawn_blocking` at the
backend.create call site (or push spawn_blocking inside `create()`
so all callers benefit).** Risk: low; effort: low.

**Priority elevation:** R23-A2 was P3-after-Phase-3 in r23. R24
recommends **P2-elevation** because:
1. R24-P1's host_dir leak math shows storage pressure that
   incentivises bursty creation (operators retrying CREATE under
   transient failure → batching → more concurrent calls on ntex
   workers).
2. Bug 1 + Bug 2 fixes have now landed in v34 / driver v14 — the
   stress-cluster will be exercising c=20 CREATE bursts in the
   imminent T-8b-stress-r3, exposing this exact ntex-block hazard.

Re-classified to **R24-A1 elevation: try_create spawn_blocking
wrap, P2 (before Phase-3 R16-P3/P5).**

## Cross-lens consensus

- **Architecture r24 / r25** (`docs/reviews/sandbox-snapshot-restore-
  architecture-2026-05-25-r24.md`, r25): no perf-relevant new items
  introduced by the v34 host_dir-GC sweeper landing. The architecture
  lens validated the leak-and-sweep contract as correct for the
  race it closes; perf-lens concurs.
- **Concurrency r24 / r25**: closed R24-I1 (terminal-overwrite warn
  threading) at `1c255a00`. Zero perf delta (single struct field
  added to a log call).
- **API-surface r23**: closed R23-API1 (`WakeErrorCode::
  StagingPathMissing`) at `79871194`. Zero perf delta (R24-M3).
- **Security r25**: confirmed `error_message` carries no host path
  (path-free `Display` impl). Wire envelope is small (R24-M4); no
  information leak / amplification vector.
- **Stress harness r24 (R24-T1)**: SHA-pin lands in
  `gcp-worker-startup.sh`; verified one-shot boot cost, not in any
  hot path. Confirmed not a perf concern.

## Carry table (r23 → r24 closure status)

| Finding | File:Line | Win | Effort | Risk | Status @ r24 |
|---|---|---|---|---|---|
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:411`, `snapshot_store.rs:184` | ~3 s/SNAPSHOT | ~80 LOC | medium | TODO (unchanged) |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT ~3.5 s + WAKE ~2 s + 95% storage | ~200 LOC | medium | TODO (unchanged) |
| **R11-P1** Per-thread pg pool | `db.rs:538-544` | ~112 ms/wake + sweeper conn-churn (R24-M1) | ~150 LOC | medium | TODO (priority sustained; now doubly-motivated — wake AND sweeper) |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC | low | TODO (unchanged) |
| **R17-P1** Skip intermediate set_state writes | `wake_machine.rs:280, 309, 401, 424, 456` | ~4-10 ms/wake | ~30 LOC | medium | TODO (unchanged) |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC | low | TODO (unchanged) |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:228`, `admin_handlers.rs:1594-1613` | ~2-6 ms/wake | ~15 LOC | ~zero | TODO (unchanged) |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms | 2 LOC | ~zero | TODO (unchanged) |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low | TODO (unchanged) |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:404-411` | instrumentation | ~5 LOC | ~zero | TODO (unchanged) |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3 s background | 1 LOC | ~zero | TODO (unchanged) |
| **R21-M1** Restoring-phase watchdog tick | `wake_machine.rs:309`, `db.rs:3207` | concurrency fix; ~5-15 ms/wake | ~30 LOC | low | TODO (unchanged) |
| **R23-A2 / R24-elevation** `try_create` spawn_blocking wrap | `nomad_ch.rs:606`, `handlers.rs:225` | ~3-5 s/CREATE ntex-worker block @ c=20 | ~30 LOC | low | TODO (priority elevated to **P2 — before Phase-3**) |
| **R24-A1 (NEW)** Tighten `HOST_DIR_GC_GRACE_SECS` default | `sweep.rs:886` | ~6× storage reduction; sweeper N_subdirs proportional | 1 LOC | ~zero | NEW carry |
| **R24-A2 (NEW)** Batch sweeper pg lookups | `sweep.rs:1038,1078` (call sites); new db helpers | ~3 s/tick → ~50 ms/tick at N=1000 | ~80 LOC | medium | NEW carry (P3) |
| ~~R23-API1~~ Typed staging error wire path | `restore_handler.rs:158`, `db.rs:1530` | API + zero-cost | — | — | **CLOSED `79871194` + `022f778a` — R24-M3** |

**Top-3 SLO wins (revised against measured baseline):**
1. **R16-P3 + R16-P5** (~5-6 s combined SNAPSHOT + WAKE win, unchanged).
2. **R23-A2 / R24-elevation** (NEW P2 — frees ntex worker through
   `try_create`'s ~3-5 s sync block under cold-boot c=20 stress).
3. **R11-P1** (priority sustained — pg-conn-cap hazard at c=20 +
   sweeper conn churn at N_subdirs≥250).

**Top-3 storage / steady-state wins:**
1. **R24-A1 (NEW)** — 1 LOC default-grace tighten; ~6× storage cut.
2. **R16-P5** — 95% storage cut on snapshot artefacts.
3. **R24-A2 (NEW)** — sweeper pg roundtrip batching at fleets >1000.

## Closures since r23

- **R23-API1 + R25-S1 + R25-I1 + R25-I2** (typed
  `SubmitRestoreError` + `WakeErrorCode::StagingPathMissing` wire
  path) — LANDED at `79871194` + `022f778a`. R24-M3 confirms zero
  hot-path cost. **CLOSED.**
- **R24-T1** (in-repo `snapshot_stress.py` + GCP startup SHA-pin)
  — LANDED at `dd2079a9` + `61492e54`. Boot-time only; not a
  controller perf cost. **CLOSED.**
- **R24-I1** (terminal-overwrite WARN error_code+message
  threading) — LANDED at `1c255a00`. Zero perf delta.

## Lens hand-off

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — unchanged.

**Tier 2.5 (1 PR, ~10-25 ms/wake + ~5-15 ms/wake POST + ~1-5 ms/
CREATE):** R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2
metric + R16-P6 (8 → 32 MiB chunk size). Unchanged composition.

**Tier 2.5b (1 PR, R21-M1 watchdog tick):** unchanged.

**Tier 2.6 (NEW, 1 PR, R24-A1 grace tighten):** **1 LOC, no risk.
Land as a tag-along to any v34-touching PR.** Cuts the host_dir
stranded-peak by 6×. **NEW.**

**Tier 2.75 (1 PR, R11-P1 priority sustained):** unchanged scope;
now doubly motivated (wake + sweeper conn churn).

**Tier 3 (2 PR, ~5-6 s SLO win on measured baseline):** R16-P3 +
R16-P5. Unchanged sequencing — gate on Bug 1 + Bug 2 cluster
validation.

**Tier 3.0a (R24-elevation, 1 PR, `try_create` spawn_blocking):**
~30 LOC, low risk. **Now P2 — moves AHEAD of Tier-3** because
T-8b-stress-r3 will hit c=20 CREATE bursts under the v34 driver
contract and the ntex worker block is the next bottleneck after
the leaky-host_dir fix lands.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT +
wake-phase-duration histogram + **(NEW) host_dir GC scanned/reaped
histogram per tick** (lets operators see N_subdirs trends as the
fleet grows).

**Tier 5 (NEW, 1 PR, R24-A2 sweeper pg-lookup batching):** ~80 LOC,
medium risk. Defer until fleets >1000.

## Carry-forward notes (focus areas requested)

### Sweeper at 1000 sandboxes

- **pg cost: bounded and indexed.** Per-tick: ~3 s pg wall + ~16 s
  pool-handshake (R11-P1 hazard) = ~19 s. Runs on dedicated
  thread, doesn't block ntex.
- **R11-P1 reinforced:** at N=250 stranded dirs, per-tick conn
  churn (500 opens / 5 min = 1.67/sec) matches a wake POST
  burst. Lands top of Tier 2.75.
- **R24-A2 batching:** cuts roundtrips 2N → 2 at fleets >1000.
  Not justified short-term.

### host_dir leak peak storage

- **Steady-state stranded: ~65 × creation_rate_per_min** (R24-P1
  math).
- **Per-dir size: ~50-500 MB** (CREATE-fail-only: ~12-50 MB;
  stop-after-wake: ~200-800 MB; mix ~200 MB; R24-M2). The r23
  carry's "~512 MB" overstated by 2-3×.
- **Realistic worst case at 10 creates/min sustained: ~130 GB
  stranded** before sweep reaps. N2-standard-32 + local SSD has
  ~300-500 GB headroom; survivable but pressures monitoring.
- **R24-A1 grace tighten = 1 LOC fix, 6× immediate reduction.**
- **R24-P1 (NEW IMPORTANT)** is the leak's primary unmonitored
  hazard; flagged for the post-cutover ops window.

### Typed error variant cost (`SubmitRestoreError`)

- **Zero hot-path cost.** No Box, no Vec, no heap. Enum
  construction is a stack move (R24-M3). Pattern-match is one
  branch in a jump table; rustc + LLVM elide the `unreachable!()`
  arm in optimized builds.
- **Failure-path cost: ~1 ns** for the extra `classify_failure`
  match arm.
- **Per-process cost: ~zero.** No allocation churn introduced.

### R23-A2 confirmation at HEAD

- **Confirmed open** at `nomad_ch.rs:629` (`try_create` async fn
  with sync syscalls inline).
- **Confirmed not wrapped at handler** (`handlers.rs:225` calls
  `backend.create(...).await` directly on ntex worker).
- **Priority elevated:** P3 → P2 (ahead of Tier-3 Phase-3
  encrypt-pass optimisations) because T-8b-stress-r3 will be the
  first c=20 burst against the v34 contract.

### Stress harness in-repo SHA verification

- **Confirmed boot-time only** (one-shot in
  `gcp-worker-startup.sh:237`).
- **Not on any per-stress, per-wake, or per-snapshot hot path.**
- **Irrelevant to controller-side perf.** Carry: none.

### wake_jobs row growth (error_code + error_message)

- **Wire envelope: ~280-320 bytes** for a failed-wake poll
  response (R24-M4).
- **Pg row growth: ~100 bytes** for the two TEXT columns combined
  in the common case.
- **TOAST risk: none** (longest realistic message ~70 bytes,
  three orders below the 2KB TOAST threshold).
- **At c=20 stress run worst case (60 failures): ~360 KB
  additional response traffic.** Trivial.

## Diagnostic-discipline carry from r23

r23 introduced the discipline: "all projections must be grounded
in measurement; explicitly mark projection-of-projection vs delta-
against-measured." R24 follows this:

- R24-P1's storage table is a **measurement-derived projection**:
  the `creation_rate × grace × per-dir-size` arithmetic uses
  measured `HOST_DIR_GC_GRACE_SECS = 3600` and **estimated**
  per-dir size from sources at `nomad_ch.rs:3656`
  (`create_ext4_image_if_missing` body) — flagged as estimate, not
  measurement.
- R24-M1's pg cost is a **per-roundtrip projection** scaled by
  measured `~1 ms` local pg RTT; the wall would shift on
  cross-AZ pg.
- R24-M2 explicitly retires the r23 carry's "~512 MB per-dir" as
  **overstated** by 2-3×.
- R24-M3 cost statement is **zero-cost provable from rustc/LLVM
  optimisation invariants**, not estimated.
- R24-M4 wire-envelope size is **measured by JSON-template
  arithmetic** against the actual response shape at
  `admin_handlers.rs:1306-1314`.

No projection-of-projection. All numbers carry their grounding.

## Net assessment

The v34 host_dir leak + sweeper landing is **architecturally
correct** for the race it closes, with **bounded but real**
storage and conn-churn costs that scale with fleet size and
creation rate. The typed-error work introduces **zero hot-path
cost**. The remaining open carries (R11-P1, R23-A2 elevated,
R16-P3, R16-P5) are unchanged in scope; **R23-A2 priority is
elevated** because T-8b-stress-r3 will exercise the ntex-worker
block under c=20 CREATE bursts that v34 finally permits.

**Two new carries (NEW): R24-A1 (1-LOC grace default tighten) and
R24-A2 (sweeper pg lookup batching, deferred).** R24-A1 is
cherry-pickable as a tag-along to any v34-touching PR; recommend
landing in the next merge.
