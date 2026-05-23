# Performance review — 2026-05-24 round 4

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `29afea72`
**Lens**: performance
**Last reviewed (this lens)**: 2026-05-24 r3 (wake p50 = 9.5 s c=1)
**New measured data**: B18-fixer c=4 stress wake **p50 = 15.0 s, p95 = 24.9 s** — a +5.5 s p50 / +15 s p95 regression vs the r3 c=1 single-shot 9.5 s.

## Summary

- **9 findings** (4 CRITICAL, 4 IMPORTANT, 1 MINOR). A3 / A2b / `compute_artifact_sha256` 64 KiB read / GCS serial download / `std::thread::sleep` — all still wide open from r3. No `spawn_blocking` added anywhere in the wake path between r3 (`03d15012`) and r4 (`29afea72`). New: the c=1→c=4 regression and a Mutex-on-poison-recovery hot-path allocation in the shared VmIndexAllocator.

## c=1 → c=4 wake p50 regression analysis (the 5.5 s gap)

Hypothesis 1 (Mutex contention on shared `VmIndexAllocator`): **refuted by code**. `backend/nomad_ch.rs:649-662` (create) and `restore_handler.rs:786-794` (restore) both call `.lock().alloc()` / `.lock().reserve()` and drop the guard within microseconds — no I/O under lock. Lock-hold ≪ 1 ms; not 5.5 s.

Hypothesis 2 (B19 slot exhaustion retries): **refuted for p50**. B19 surfaces *after* ~10-12 successful wakes; the cluster c=4 run reached 5/5 wakes before exhaustion. p50 across only-5 samples cannot be inflated by an exhaustion that has not yet happened. p95/p99 tail in longer runs *will* be dominated by it once B19 fires.

Hypothesis 3 (parallel sync I/O on the ntex worker future serializes): **confirmed by code**. `restore_handler.rs:286-412` runs every blocking call directly on the request future (no `spawn_blocking` — verified: `grep spawn_blocking restore_handler.rs` returns zero matches in the wake path; the only hit at `:651` is a stale comment). `LocalDiskSnapshotStore::get` (`snapshot_store.rs:227-252`) issues a 2-pass 1 GB read+copy per call; `wait_for_alloc_running_blocking` (`:1146`) `std::thread::sleep(250 ms)` and `wait_for_livez_blocking` (`:1179`) `std::thread::sleep(150 ms)` park the **kernel thread**, not the future. Under c=4 four such futures land on at most a handful of ntex workers (worker count = CPU on a 2-vCPU worker → 2). With sleeps parking the thread, **the second wake on the same worker waits for the first wake's full sleeps + sync I/O to complete**. Disk read bandwidth itself is also shared: 4×1 GB SHA + 4×1 GB copy on one disk = sequential ~6 GB through one I/O queue → ~4-8 s of pure I/O contention beyond the c=1 baseline. Sum: ≈ 5-7 s of additional wall, consistent with the measured +5.5 s p50 / +15 s p95.

**Root cause: A3 (sync I/O on compio worker) compounds super-linearly under concurrency.** Closing A3 closes this regression.

## CRITICAL

1. **`crates/sandbox/src/restore_handler.rs:286-412,1146,1179` — A3 still wide open at r4 HEAD `29afea72`.** No `spawn_blocking` wraps `store.get`, `submit_restore_job`, `wait_for_alloc_running_blocking`, or `wait_for_livez_blocking`. `std::thread::sleep(250 ms)` (`:1146`) and `std::thread::sleep(150 ms)` (`:1179`) park the ntex worker thread, not the future. Under c=4 this serializes wakes on each worker. **This is the measured 5.5 s p50 / 15 s p95 regression.** Fix unchanged from r3: wrap blocking calls in `compio::runtime::spawn_blocking`; replace `std::thread::sleep` with `compio::time::sleep`. Until then, the cluster wake SLO (§ 10.2: p50 ≤ 1 s; p95 ≤ 1.5 s) is unreachable at any concurrency.

2. **`crates/sandbox/src/snapshot_store.rs:227-252` — `LocalDiskSnapshotStore::get` does 2× full 1 GB sequential passes per call; under c=4 these passes contend on the same disk.** Line 239 hashes 1 GB (SHA-bound ~500 MB/s on one core); line 249 copies 1 GB (read+write, ~1 GB through disk). With c=4 the disk queue serializes 4× = ~6 GB sequential through one device → +4-8 s wall on the slowest concurrent wake. Replacement with `std::fs::hard_link` (same FS root in the cluster layout — L1 dir and `alloc_dir` both under `host_state_dir`) eliminates the copy pass entirely; parallel SHA across the 4 wakes via `compio::runtime::spawn_blocking` per-file gives near-linear-speedup on a 2-vCPU box. **Single highest-ROI fix for the c=4 wake tail.**

3. **`crates/sandbox/src/snapshot_handler.rs:316-373` — `do_snapshot_inner` runs synchronously on the ntex future.** `ch.pause` (`:335`), `ch.snapshot` (`:337`), `store.put` (`:345`) all blocking; `compio::runtime::spawn_blocking` wraps **none** of them. CH snapshot is ~2 s + `put` is ~6 s for 1 GB (SHA + move). Snapshot under load contends with concurrent wakes on the same ntex worker — second-order regression once idle-eviction runs concurrent with wakes. Same fix shape as A3.

4. **`crates/sandbox/src/snapshot_store_gcs.rs:545-564,920-930` — A2b unmoved.** L1-miss → L2 wake path serial-downloads 3 files (`:553`) **and** L2 `verify` re-streams the full 1 GB artifact (`:608-611`). Once L1 evicts (TieredSnapshotStore at 32 GB cap → ~32 sandboxes), a wake on a cold worker pays 1 GB GCS egress on top of the local-I/O budget. r2 / r3 actions still apply: parallel per-file download via `compio::runtime::spawn` + `x-goog-meta-sha256` fast-path for `verify`.

## IMPORTANT

5. **`crates/sandbox/src/snapshot_store.rs:153-185` — `compute_artifact_sha256` uses unbuffered 64 KiB `File::read`.** Carried r1/r2/r3. 1 GB = 16,384 syscalls; one-line `BufReader::with_capacity(1 << 20, f)` cuts that to ~1024 and moves SHA throughput from syscall-bound to memory-bound (~1 GB/s). Disproportionately effective once A3 is wrapped in `spawn_blocking` — cheap and orthogonal.

6. **`crates/sandbox/src/snapshot_store_gcs.rs:493-543` — GCS `put` 3 reads + 2 SHA passes per artifact.** Carried r1/r2/r3. On 1 GB this is ~6 s of single-core work pre-upload. Snapshot path; affects idle-eviction throughput rather than wake — but the slower snapshots overlap with wakes on the same ntex workers, contributing to wake p95.

7. **`crates/sandbox/src/backend/nomad_ch.rs:649-662, 803-808` + `restore_handler.rs:786-794` — `Mutex` allocator code paths take `.lock().unwrap_or_else(|p| p.into_inner())` 6× in the hot path.** Each call instantiates a new closure object on the stack and clones the inner allocator state on poison-recovery. Not catastrophic — but every wake now traverses *both* the create-side and the restore-side reserve(), each with this dance. Consider `parking_lot::Mutex` (no poisoning at all) so the path becomes `.lock().alloc()` — saves one closure per lock + simplifies callers (the entire crate would benefit; ~25 call sites with `unwrap_or_else(|p| p.into_inner())`).

8. **`crates/sandbox/src/restore_handler.rs:331-346` — diagnostic 3× `metadata()` immediately after `store.get`.** Carried r3. The bug it diagnosed (B14a) is CLOSED per deferred file. The 3 syscalls fire on the wedged future before any of the heavier waits — small but reproducible from wrapper `ls -la`. Drop or downgrade to `tracing::debug!`.

## MINOR

9. **`crates/sandbox/src/snapshot_aead.rs:311-316` — 13-byte `Vec` alloc per 1 MiB AEAD chunk.** Carried r1/r2/r3. Dormant: A1 (AEAD never wraps prod store) still open per deferred file. When A1 closes, this adds ~1024 allocs per 1 GB artifact on every put/get — pre-stack `[u8; 13]` before A1 lands or the wake budget gains another ~1 s of AEAD setup cost.

## Two most critical citations

- `crates/sandbox/src/restore_handler.rs:286-412` + `:1146,1179` — entire wake path sync I/O + `std::thread::sleep` on the ntex future. Confirmed via code reading: zero `spawn_blocking` calls in this file's wake path between r3 and r4. **This is the c=1→c=4 regression's root cause: parking the kernel thread serializes concurrent wakes on each worker.**
- `crates/sandbox/src/snapshot_store.rs:227-252` — `LocalDiskSnapshotStore::get` 2× 1 GB sync passes. Under c=4, contend on one disk queue → +4-8 s wall on the slowest concurrent wake. Replacing `std::fs::copy` with `std::fs::hard_link` eliminates the copy pass entirely (same `host_state_dir` root in cluster layout); biggest single-line ROI on the c=4 tail.
