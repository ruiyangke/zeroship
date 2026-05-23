# Performance review — 2026-05-24 round 7

**HEAD**: `6f5d41b8` (B22 clock_resync) — incl. `4c090992` R6-P1.
**New data**: Appendix F c=4 1+1: snapshot p50=50573 ms; wake p50=9235 ms (was 9729 ms pre-#22).

## Summary

**8 findings** (3 CRITICAL, 4 IMPORTANT, 1 MINOR). R6-P1 detach landed correctly in source — but **snapshot p50 did not move (50307 → 50573 ms)**. The r6 root-cause ranking ("90% teardown") was right for c=1 and wrong for c=4. Detaching the teardown only exposed the next bottleneck: the pre-detach sync work (`ch.pause` / `ch.snapshot` / `store.put`) saturates the 4 ntex workers under c=4 SSD contention.

## R6-P1 effectiveness audit

**Implementation: VERIFIED CORRECT.** `admin_handlers.rs:1310-1324` is `Arc::clone(&state)` + `compio::runtime::spawn(async move {…}).detach()`. No await on the join handle. The pg CAS to `snapshotted` happens inside `snapshot_handler::snapshot_sandbox` (`snapshot_handler.rs:349-359`) BEFORE this point, so the artifact is durable at detach. The diff is in HEAD (`git merge-base --is-ancestor 4c090992 6f5d41b8` ⇒ true); Appendix F's v17 was built at `6f5d41b8` per cluster doc :1108-1112.

**Effect: zero on c=4 p50.** Brief's hypothesis (2) — "50s is the CH-pause + memory-dump path, NOT teardown" — **CONFIRMED**. Hypotheses (1) wrong-binary and (3) detach awaited elsewhere are REJECTED. R6's c=1→c=4 extrapolation was wrong: at c=4 on n2-standard-4, four sync ch-remote 2 GB memory dumps compete for the same local SSD; effective per-stream throughput collapses to ~40-50 MB/s → ~40-50 s per snapshot, which matches the observed p50 exactly. R6-P1 is still worth keeping (it removes a serial ~45 s from the c=1 budget; future c=1 smoke will show that); it just isn't sufficient under contention.

## CRITICAL

1. **`snapshot_handler.rs:316-373 + 566-621` — pre-detach sync work parks an ntex worker for 30-50 s under c=4.** `snapshot_sandbox` invokes `ch.pause` (`:335`), `ch.snapshot` (`:337`), `store.put` (`:345-347`) directly. `run_with_timeout` (`:566-621`) `std::process::Command::spawn`s ch-remote then `std::thread::sleep(50ms)` busy-polls `try_wait`. **No `spawn_blocking`.** The trait doc at `:99` ("callers that need async should hop through `compio::runtime::spawn_blocking`") is violated at every call site. This is the entire 50 s budget at c=4; `spawn_blocking` is the fix.

2. **`snapshot_store_gcs.rs:832-859` — detached L2 GCS uploads use `compio::runtime::spawn`, not `spawn_blocking`.** Each `TieredSnapshotStore::put` (`:814`) detaches an async task that calls the sync GCS `SnapshotStore::put`. Every concurrent snapshot pins a compio runtime worker for the 6-8 s of TLS + 1 GB PUT, while also reading 1 GB off the same SSD the *next* snapshot's `ch.snapshot` is writing to. Worker-pool starvation + cross-snapshot disk-read contention. Wrap the inner `l2.put` (`:836`) in `spawn_blocking`.

3. **`backend/nomad_ch.rs:977-1123` — R6-P1's detached teardown task occupies a compio task slot for 50-150 s.** Not a p50 regression but a steady-state ceiling: under sustained c=4 burst, prior cycles' background teardowns are still resident when new snapshots arrive (`wait_for_job_gone` 30 s + `wait_for_agent_silent` up to `host_fence_timeout_secs`=120 s). Cap detached teardowns at runtime or accept the resource overlap is bounded by sandbox lifetime.

## IMPORTANT

4. **`restore_handler.rs:1326-1340 + ~1391-1395` — `wait_for_alloc_running_blocking` (250 ms poll) and `wait_for_livez_blocking` (150 ms poll) use `std::thread::sleep` inside the async handler.** Same shape as snapshot's busy-poll. Wake p50 9235 ms estimated breakdown: `submit_restore_job` ~500-800 ms; `wait_for_alloc_running` ~3-5 s; `wait_for_livez` ~2-3 s; `store.get` 1 GB SHA verify (`snapshot_store.rs:246`) ~1.5-2 s; `clock_resync_post_restore` ~100-200 ms. Moving the two poll waits to `compio::time::sleep` drops 1-2 s c=1, 3-5 s c=4.

5. **In-tree uncommitted draft at `restore_handler.rs:386-400` adds `Arc<dyn>` + `spawn_blocking(move || store_clone.get(...))` — but was NOT in v17.** `git blame` shows `Not Committed Yet 2026-05-23 02:41:36`; v17 built at 02:19:26. So Appendix F's wake p50 reflects the pre-spawn_blocking path. Once committed and shipped in v18, expect c=1 wake p50 -1.5-2.5 s and c=4 -3-5 s (re-confirms r6 A3 slice estimate).

6. **`restore_handler.rs:513 + ~1559-1576` — `clock_resync_post_restore` wall cost ~100-200 ms** (Ed25519 sign + TCP + agent verify-bypass + `settimeofday` + 200 OK). Resync RPC ≈ 2% of wake p50. Wake p50 actually dropped 494 ms post-#22 (likely small rootfs/agent startup gains swamping the resync cost). Acceptable.

7. **`snapshot_handler.rs:570 + 578` — ch-remote's stdout is `Stdio::piped` but never read until exit.** If ch-remote writes >64 KB stdout (pipe buffer), it blocks on write while our `try_wait` loop sees nothing; we hit the 30 s timeout instead of surfacing the real error. Switch stdout to `Stdio::null()` or drain on a reader thread.

## MINOR

8. **`snapshot_store_gcs.rs:308, 462, 633` — three more 64 KiB unbuffered loops** (carried r6#5). Symmetric `BufReader<1 MiB>` fix to `snapshot_store.rs:175`. ~125 ms wall savings per GCS verify.

## Post-R6-P1 snapshot p50 budget (re-ranked, c=4 n2-standard-4)

1. **Sync `ch.pause` + `ch.snapshot` ch-remote subprocess dumping 2 GB guest RAM with c=4 SSD contention (~35-45 s)** — finding #1. ~85% of budget.
2. **`compute_artifact_sha256` on L1 put (~1.5-2.5 s)** — finding #1 same site.
3. **L2 GCS upload cross-snapshot SSD-read contention added to #1 (~3-5 s)** — finding #2.
4. **pg CAS + status updates (~50-100 ms)** — negligible.
5. **R6-P1 detached teardown (~0 ms on p50 path; 50-150 s in background)** — finding #3. Was 90% in r6; now 0% in p50.

## Two most critical citations

- `snapshot_handler.rs:316-373` (esp. `:335 :337 :345`) + `:566-621` — **ChRemoteClient + LocalDiskSnapshotStore::put run synchronously on the async caller; trait doc `:99` says wrap in `spawn_blocking`; no caller does.** Post-R6-P1 long pole.
- `snapshot_store_gcs.rs:832` — **detached L2 upload is `spawn`, not `spawn_blocking`** — each concurrent snapshot leaves a background compio task parked on 1 GB of sync GCS PUT.
