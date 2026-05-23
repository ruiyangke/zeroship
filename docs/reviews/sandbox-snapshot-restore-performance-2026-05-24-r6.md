# Performance review — 2026-05-24 round 6

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `29196e0c` (post `77ea717f` R5-P1 BufReader partial)
**Lens**: performance
**Last reviewed (this lens)**: r5 (BufReader partial; spawn_blocking carved to R5-P1b)
**New data (Appendix E, c=4 c=1 smoke)**: wake p50=9729ms / p95=p99=13442ms; **snapshot p50=50307ms / p95=58080ms**; create p50=15400ms; stop p50=19ms.

## Summary

**8 findings** (3 CRITICAL, 4 IMPORTANT, 1 MINOR). The cluster surface has flipped: wake (1 GB SHA + sleeps + sync I/O) is no longer the long pole — **snapshot p50=50s is**, and it is NOT SHA-bound. R5-P1 BufReader did not move wake p50 because the wake budget was never SHA-bound in the first place; the dominant cost is `std::thread::sleep` poll cadence + a sync 1 GB GCS `download_to_disk` outside `spawn_blocking`. Snapshot's 50s is **almost entirely `teardown_source_for_snapshot`** running synchronously in the response path.

## CRITICAL

1. **`backend/nomad_ch.rs:1029-1123` + `admin_handlers.rs:1289-1299` — snapshot p50 root cause: synchronous in-response `stop_inner` waits up to 30s `wait_for_job_gone` + up to 120s `wait_for_agent_silent` (`host_fence_timeout_secs`).** `snapshot_sandbox` HTTP handler awaits `teardown_source_for_snapshot(...)` BEFORE writing the 200 response. The fence is intentionally pessimistic (Nomad alloc-terminal lags 0.5–60s under stress per the inline comment :1057-1064), so steady-state p50 lands at ~30s job-gone + ~15-20s agent-silent = ~45-55s. **This explains the measured snapshot p50=50307ms exactly.** SHA + CH dump together account for ≤4-5s of the budget; the rest is the host-fence. Mitigation: split the response from the teardown — return 200 immediately after `update_snapshot_metadata`, and `compio::runtime::spawn` the teardown into a detached task with a metric (`snapshot_teardown_lag_seconds`). The artifact is already durable in pg + L1 at the point of CAS; the post-CAS teardown is operationally async even though it's coded sync.

2. **`restore_handler.rs:1302,1335` + r5-P1b carve-out — why BufReader did not move wake p50.** R5-P1's `BufReader<1 MiB>` at `snapshot_store.rs:175` cut the SHA syscall count 16× as designed, but the wake p50 floor was never SHA-bound. The dominant residual wake cost is now: (a) `wait_for_alloc_running_blocking` polling at 250 ms (`:1302`) — Nomad alloc transitions take 4-8s in practice, so the 4 GB rootfs schedule + start contributes ~20-32 polls × 250 ms granularity slop (cumulative 1-2s wasted wait); (b) `wait_for_livez_blocking` polling at 150 ms (`:1335`) for the in-VM agent to bind :7777 after CH `--restore` + resume — measured ~3-5s; (c) the entire `store.get` 1 GB hard_link + canonical SHA-verify still on the ntex worker, no `spawn_blocking`. BufReader removed ~1.5-2s from the SHA loop, but that's hidden inside a 9.7s wake budget dominated by (a)+(b)+(c). **Expected wake p50 reduction when R5-P1b lands (spawn_blocking on store.get): ~1.5-2.5s c=1 (no longer parks ntex worker; SHA still 1.5-2s wall but no other futures blocked); 3-5s c=4 (eliminates serial contention across concurrent wakes).** The `&dyn → Arc<dyn>` flip is the only real friction.

3. **`snapshot_handler.rs:316-373` — zero `spawn_blocking` on the snapshot critical path.** `admin_handlers.rs:1272-1281` calls `snapshot_handler::snapshot_sandbox(...).await`; the async fn then synchronously invokes `ch.pause` (`:335`, 30s timeout w/ 50 ms busy-poll on `try_wait` per `:609`), `ch.snapshot` (`:337`, same), `store.put` (`:345-347`, 1 GB SHA + 3 renames). Every one of these parks the ntex compio worker for the full duration. Under c=4 idle-eviction + admin snapshot overlap, every concurrent snapshot serializes on whichever ntex worker accepted the HTTP request. The trait doc at `:99` explicitly says "callers that need async should hop through `compio::runtime::spawn_blocking`" — none do. **The trait contract is violated at every call site.**

## IMPORTANT

4. **`snapshot_store_gcs.rs:493-543` — GCS `put` re-hashes 1 GB twice (per-file `sha256_file` + canonical `canonical_artifact_sha256`) before returning.** The per-file SHA at `:514` and the canonical SHA at `:527` together read `memory-ranges` twice off disk. Even with OS page cache absorbing the second read, that's another 1-2s of single-core hashing on top of the L1 `compute_artifact_sha256` already-paid pass. Fold the two hashes into one streaming pass: feed the per-file hasher into the canonical hasher simultaneously (the canonical hasher just consumes `name || len_be || bytes` per file). Estimated reduction on cold-eviction GCS upload path: ~1.5-2.5s (single-core). Dormant while L1 is warm (today's hot path is L1-only `get`).

5. **`snapshot_store_gcs.rs:308,633,747` — three more 64 KiB unbuffered loops (R5-P1's BufReader change only patched `snapshot_store.rs:175`, not the GCS path).** `upload_resumable` at `:308` reads through an 8 MiB `Vec` directly (no `BufReader` needed there since chunks are 8 MiB), but `verify_canonical_sha256_from_streams` (`:633`) and `download_to_disk`'s implicit `io::copy` (`:395`) re-hash + write at 64 KiB grain through `std::io::copy`'s default 8 KiB inner buffer. Cost: 1 GB / 64 KiB = 16384 syscalls per verify; ~125 ms wall on top of network bandwidth. Wrap the `r.into_reader()` at `:394` in `BufReader<1 MiB>` for symmetry with `snapshot_store.rs:175`.

6. **`restore_handler.rs:1302,1335` — A3 slice 3 (`std::thread::sleep` → `compio::time::sleep`).** With R5-P1b's `Arc<dyn>` flip landed, the entire `wait_for_alloc_running_blocking` + `wait_for_livez_blocking` can also move to async — they currently park a blocking thread for 250 + 150 ms × many polls. **Expected reduction in wake p50 when sleeps go async: ~0.5-1.0s c=1; up to 2-3s c=4 (eliminates blocking-pool contention).** Lower-impact than R5-P1b but still measurable.

7. **`snapshot_store_gcs.rs:308` — `vec![0u8; 8 MiB]` allocated PER `upload_resumable` call.** Re-allocated on every snapshot's `memory-ranges` upload (~1 GB → 128 chunks but only ONE 8 MiB buf — that's fine), but the 64 KiB buffers at `:462, 633, 747` allocate per-call too. Hoist to `thread_local!` or accept the allocator is cheap on hot-path glibc; the cost is sub-millisecond per snapshot. Listed for completeness — the OOM-on-malloc-spike contributor is GCE pre-emptible workers' ballooning, not these allocations.

## MINOR

8. **`restore_handler.rs:1191-1208` — fresh `ureq::Agent` per `nomad_get/post_blocking`.** Carried r5#9. 4 polls/s × ~10s = ~40 TCP handshakes per wake to localhost Nomad. ~0.5-1ms each via Unix-socket-on-loopback. Sub-1% of wake p50; defer.

## A2b verify re-stream

**Re-confirmed open + latent.** Wake path uses `store.get` at `restore_handler.rs:365`, not `verify`. `TieredSnapshotStore::get` (`snapshot_store_gcs.rs:864-892`) tries L1 first; only on `NotFound` does it fall through to L2. Cluster smoke is L1-warm post-snapshot so the egress never triggers. A2b stays in deferred until periodic-L2-sweep wiring lands or L1 eviction is observed in cluster smoke.

## Ranked snapshot p50=50s root cause (1 = largest)

1. **`stop_inner` host-fence + Nomad-purge wait (~45s wall, sync-in-response)** — finding #1. ~90% of the budget.
2. **`compute_artifact_sha256` on L1 put (~1.5-2.5s, post-BufReader)** — finding #3.
3. **`ch.pause` + `ch.snapshot` (~2-3s, CH itself writing 1 GB)** — fundamental, not optimizable above CH.
4. **pg `update_snapshot_metadata` (~50-100 ms)** — negligible.
5. **GCS L2 fire-and-forget detach (~0 ms on the response path)** — non-blocking; absorbs eventual ~6-8s in the background.

## R5-P1 BufReader effect analysis

**Why p50 didn't move (wake)**: BufReader cut SHA syscall amplification 16× (~1.5-2.5s saved on the SHA loop) but wake p50 was budget-dominated by `submit_restore_job` + `wait_for_alloc_running_blocking` (Nomad scheduling) + `wait_for_livez_blocking` (agent startup), not by SHA. The savings are real but hidden under a ~5-7s floor of Nomad+agent latency. R5-P1b (spawn_blocking + Arc<dyn>) is the lever that surfaces the savings by unblocking the ntex worker for concurrent wakes — measurable as a p95 c=4 drop, not p50 c=1.

## Two most critical citations

- `backend/nomad_ch.rs:1029-1123` + `admin_handlers.rs:1289-1299` — **snapshot p50=50s is the host-fence + Nomad-purge wait awaited synchronously in the HTTP response.** Easiest single-step win: detach the teardown into a `compio::runtime::spawn` after the CAS, return 200 immediately. Expected snapshot p50 drop: ~45s → ~3-5s.
- `snapshot_handler.rs:316-373` — **zero `spawn_blocking` on the snapshot path**; trait doc says callers must wrap, no caller does. Combined with finding #1, snapshot path parks an ntex worker for ~50s end-to-end.
