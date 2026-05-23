# Performance review — 2026-05-24 round 3

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `03d15012`
**Lens**: performance
**Last reviewed (this lens)**: 2026-05-24 r2
**New measured data**: cluster smoke c=1 wake = **9.5 s** (HTTP 200); §10.2 SLO target p50 ≤ 1 s.

## Summary

- 7 findings (3 CRITICAL, 3 IMPORTANT, 1 MINOR). All known r1/r2 perf items still open; no closures this round.
- The 9.5 s wake on the live HTTP handler path is a direct, measured consequence of r1 [A3]: `restore_handler::do_restore_inner` runs entirely on the compio HTTP worker future, dominated by `LocalDiskSnapshotStore::get` (sync 1 GB SHA + 1 GB copy) and two `std::thread::sleep` poll loops.

## Root-cause budget for the 9.5 s wake (cluster `dec489a1`, c=1, L1 hit)

Path: `admin_handlers::wake_sandbox` → `restore_handler::restore_sandbox` → `do_restore_inner`. Each line below is a sync block on the **live ntex worker future** — no `spawn_blocking`, no `compio::time::sleep`. Ranked by contribution; rough budget allocation only.

| Step | File:line | Est. share of 9.5 s | Notes |
|---|---|---|---|
| 1. `store.get` — SHA-verify (1 GB) then copy (1 GB) | `snapshot_store.rs:239,249` | **~3–5 s** | `compute_artifact_sha256` (64 KiB unbuffered reads, single core, ~500 MB/s) + `std::fs::copy` of memory-ranges. Two full passes per A3. |
| 2. Wrapper post-spawn: CH mmap + `ch-remote ping` poll + `ch-remote resume` + tap UP transition | `nomad-vm-wrapper.sh:366-419` | **~1–2 s** | Cluster log: api ready at attempt=2 (≈400 ms), LOWER_UP at +300 ms post-resume. Bounded by 50×200 ms ping budget; observed ≈700 ms. |
| 3. `wait_for_alloc_running_blocking` (Nomad → ClientStatus=running) | `restore_handler.rs:1040-1098` | **~1–2 s** | `std::thread::sleep(250 ms)` between polls (`:1088`); first poll only fires post-`nomad_post`. |
| 4. `wait_for_livez_blocking` (in-VM agent `/livez` 200) | `restore_handler.rs:1108-1127` | **~1–3 s** | `std::thread::sleep(150 ms)` between polls (`:1121`). Bound by in-VM `/sbin/init` → agent bind on `:7777`. |
| 5. `remove_dir_all` + `create_dir_all` on `alloc_dir` | `restore_handler.rs:302-313` | <100 ms | Negligible unless previous alloc dir has many files. |
| 6. Post-`store.get` diagnostic 3× `metadata()` stat | `restore_handler.rs:331-346` | <10 ms | Diagnostic, but adds extra opens to a sync hot path. |
| 7. `rewrite_config_json` (JSON read/serialize, 3 KiB) | `restore_handler.rs:378-380` | <10 ms | Negligible. |

Sum: ≈ 6–12 s wall, consistent with observed 9.5 s. The dominant tail is steps 1 + 4. A2b (verify re-stream) is **dormant on the cluster wake path** — `restore_handler.rs:323` calls `store.get`, not `verify`, so no extra GCS egress today; it activates only when a periodic verify sweep wires up.

## CRITICAL

**`crates/sandbox/src/restore_handler.rs:286-412` (entire `do_restore_inner`) + `:1088`, `:1121` — wake path runs ~6–10 s of sync I/O on the compio HTTP handler future.**
`admin_handlers::wake_sandbox:1264` awaits `restore_sandbox` directly; `restore_sandbox:193` awaits `do_restore_inner` with no `spawn_blocking`. Inside, every step is blocking: `std::fs::remove_dir_all` (`:303`), `create_dir_all` (`:309`), `store.get` (`:323`, ~3–5 s SHA+copy), `rewrite_config_json` sync JSON read (`:378`), `submit_restore_job` ureq POST + `wait_for_alloc_running_blocking` (`:798`) which calls `std::thread::sleep(250 ms)` (`:1088`), `wait_for_livez` (`:805-815`) which calls `std::thread::sleep(150 ms)` (`:1121`). Net effect: the entire single-threaded compio worker that serves admin HTTP is wedged for the full wake duration; concurrent admin requests queue. **This is the measured 9.5 s — every step except the wrapper itself is on the wrong runtime.** Fix shape per r1 A3: wrap `store.get` + `submit_restore_job` + `wait_for_livez` in `compio::runtime::spawn_blocking`, and replace `std::thread::sleep` with `compio::time::sleep` so the future can yield.

**`crates/sandbox/src/snapshot_store.rs:227-252` — `LocalDiskSnapshotStore::get` does 2× full 1 GB sequential passes synchronously.**
Line 239 calls `compute_artifact_sha256(&src)` (read 1 GB, hash). Line 249 calls `std::fs::copy(src.join(name), target_dir.join(name))` (read 1 GB, write 1 GB). Single-core SHA-256 caps around 500 MB/s on this hardware; the copy adds ≥1 read + 1 write. **Combined I/O = 3 GB sequential through one core ≈ 3–5 s** of the 9.5 s budget. Two structural fixes available, both cheap:
- Replace the copy with `std::fs::hard_link` (same FS, same inode → 0-cost "copy" because the L1 root and the restore alloc_dir live under the same `host_state_dir`). Even on a different mount, switch to `reflink_or_copy` (the `reflink` crate) to leverage XFS/btrfs CoW.
- Verify SHA in parallel with the link via `compio::runtime::spawn_blocking` per file (3 independent hashes streaming concurrently across cores).
Either alone drops step 1 below 1 s; both together drop it to <500 ms.

**`crates/sandbox/src/snapshot_store_gcs.rs:545-564` — GCS `get` still serializes 3 file downloads.**
Carried from r2. Loop at `:552` walks `ARTIFACT_FILES` and calls `download_to_disk` synchronously per file. memory-ranges (~1 GB) blocks the two small files (config.json ~3 KB, state.json ~100 KB). Once L1 misses fall through to L2, this serial chain adds the full 1 GB GCS pull to the wake path — the c=4 smoke already shows wake p50 = 13.2 s (a 3.7 s degradation vs L1-hit c=1), most of which is GCS round-trip on cold-cache slot reuse. Parallelize with `compio::runtime::spawn` per file + join (no semantic risk; the post-download canonical-SHA on `:556` is the natural join point).

## IMPORTANT

**`crates/sandbox/src/snapshot_store_gcs.rs:493-543` — GCS `put` still 3 reads + 2 SHA passes per artifact** (carried r1, r2). Lines 514 (`sha256_file`) → 519 (`upload_resumable` re-reads bytes) → 527 (`canonical_artifact_sha256` re-reads + re-hashes). **This is the 22 MB/s effective throughput observed on the 48 s / 1.07 GB snapshot**: on the 1 GB memory-ranges file alone, this is ~3 GB disk read + ~2 GB hashed = ~6 s of single-core work even before GCS upload begins. Compute SHA once during the upload byte stream (Adler/tee pattern); save 2× read pass + 1× hash pass.

**`crates/sandbox/src/snapshot_store.rs:153-185` — `compute_artifact_sha256` uses 64 KiB unbuffered `std::fs::File::read`** (carried r1, r2). On the 1 GB memory-ranges file that's 16,384 read syscalls + per-call hasher updates. `BufReader::with_capacity(1 << 20, f)` collapses to ~1024 syscalls; the SHA throughput becomes memory-bandwidth-bound (~1 GB/s) rather than syscall-bound. **Cheap win on the hot path**: cuts step 1 of the wake budget by an additional ~30–40%.

**`crates/sandbox/src/restore_handler.rs:331-346` — post-`store.get` diagnostic adds 3 sync `metadata()` calls after the artifact is already staged.**
Bug-#14a diagnostic from 2026-05-22; the bug is now closed (B14a CLOSED per deferred file). The 3 extra opens are dwarfed by step 1, but they happen on the same wedged future and the trace info is reproducible via `ls -la` from the wrapper. Drop the block (or downgrade to `tracing::debug!` so opt-in only).

## MINOR

**`crates/sandbox/src/snapshot_aead.rs:311-316` — 13-byte `Vec` alloc per 1 MiB AEAD chunk** (carried r1, r2). Currently dormant: A1 still open → `AeadSnapshotStore` is never wired in prod (`lib.rs:572-622`), so the cluster wake skipped AEAD entirely. Closing A1 will surface this — pre-stack-allocate `[u8; 13]` before A1 lands or the 9.5 s budget gains another ~1 s of AEAD cost.

## r2 closure status

- r2 CRITICAL #1 (`verify` re-stream 1 GB on L1 eviction): **dormant** — no caller invokes `verify` on the wake path today (`restore_handler.rs:323` uses `get`, which verifies the local SHA post-download for free). Stays open against the future periodic-L2-sweep landing.
- r2 CRITICAL #2 (`run_idle_eviction_once` serial + sync I/O): **open**, unchanged.
- r2 IMPORTANT/MINOR: all open.

## Two most critical citations

- `crates/sandbox/src/restore_handler.rs:286-412` + `:1088,1121` — entire wake path is sync I/O + `std::thread::sleep` on the compio HTTP worker future; this is **the** measured 9.5 s root cause.
- `crates/sandbox/src/snapshot_store.rs:227-252` — `LocalDiskSnapshotStore::get` does 2× full 1 GB sequential passes synchronously; replace the copy with `hard_link`/reflink and the SHA disappears into post-link parallel verify. Single highest-ROI fix on the wake hot path.
