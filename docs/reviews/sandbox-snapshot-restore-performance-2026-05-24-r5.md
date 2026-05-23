# Performance review — 2026-05-24 round 5

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: `3e8bfad5`
**Lens**: performance
**Last reviewed (this lens)**: 2026-05-24 r4 (c=4 wake p50=15.0s, p95=24.9s)
**Closed since r4**: A3-partial — `std::fs::copy → std::fs::hard_link` at `0aa93a0f` (`snapshot_store.rs:259-281`).
**New data**: B20 cluster smoke — every cold-boot create 503s `create_retry_budget_exhausted`; `wait_for_agent_livez` times out at 30s × 3; taps `<NO-CARRIER>` on all 10 zsbx-nm-* devs.

## Summary

**9 findings** (3 CRITICAL, 4 IMPORTANT, 2 MINOR). A3 closure ~25% by lines, ~40% by wake budget. Post-hard_link c=1 wake p50 estimate (unmeasured): ~6-9s; c=4 p50 ~10-12s (down from 15s). SLO target (p50 ≤ 1.0s) still unreachable until full A3.

## CRITICAL

1. **`restore_handler.rs:327-496,1302,1335` — A3 ~75% open.** Zero `spawn_blocking` in the wake path: `store.get` (`:365`), `submit_restore_job` (`:425`), `wait_for_alloc_running_blocking` (`:971`), `wait_for_livez_blocking` (`:988`), `rewrite_config_json` (`:421`), `register_restored` (`:456-463`) all run on the ntex worker future. `std::thread::sleep(250/150ms)` at `:1302,1335` park the kernel thread. With hard_link removing the copy, the two sleep-loops + 1GB SHA are the dominant residue.

2. **`snapshot_store.rs:153-185` — `compute_artifact_sha256` 64 KiB unbuffered reads now the single largest wake cost.** Hard_link killed the copy at `:259`, SHA at `:239` remains. 1 GB = 16,384 syscalls + per-call hasher updates, ~1.5-2.5s single-core. One-line `BufReader::with_capacity(1 << 20, f)` at `:168` → ~1024 syscalls, syscall→memory-bandwidth bound. **Highest-ROI single-step now.**

3. **`nomad-vm-wrapper.sh:252-257` — cold-boot `cp --reflink=auto` of 1 GB rootfs-slim.img per alloc.** `--reflink=auto` falls through to plain copy on non-CoW FS (GCE default ext4). Cold-cache workers issue 1 GB read + 1 GB write per create. With parallel `truncate -s 20G + mkfs.ext4 -q -F workspace.img` (`nomad_ch.rs:3107-3132`), cold-boot disk queue gets ~1 GB cp + 20 GB sparse truncate + ext4 superblock writes. Likely B20 contributor (unconfirmed; worker artifacts pending pull). Mitigation: `hard_link` to a read-only rootfs template when on the same FS.

## IMPORTANT

4. **`snapshot_store_gcs.rs:545-564,608-611` — A2b GCS serial 3-file download + verify re-stream open.** Dormant on wake (`restore_handler.rs:365` calls `get`, not `verify`); activates on L1 evict or periodic L2 sweep. Parallelize via `compio::runtime::spawn`; add `verify_metadata_only` fast-path over `x-goog-meta-sha256`.

5. **`snapshot_store_gcs.rs:493-543` — GCS `put` 3 reads + 2 SHA passes per artifact.** Carried r1-r4. ~6s single-core on 1 GB pre-upload. Affects idle-eviction; second-order on wake p95 via ntex contention when snapshot+wake overlap.

6. **`snapshot_aead.rs:380-451,515-559` — AEAD stream un-buffered + per-chunk 13-byte `Vec` alloc (`:311-316`).** `dst` bare `File`, 2 write syscalls per chunk → ~2048 writes per 1 GB; `BufWriter::with_capacity(4 << 20, dst)` collapses to ~256. Dormant until A1 closes.

7. **`snapshot_handler.rs:316-373` — `do_snapshot_inner` fully sync on ntex.** `ch.pause/snapshot` (`:335,337`), `store.put` (`:345`) all blocking. ~2s + ~6s on 1 GB. Idle-eviction now overlaps with wakes on shared ntex workers.

## MINOR

8. **`restore_handler.rs:373-388` — diagnostic 3× `metadata()` post-`store.get` on wedged future.** B14a CLOSED; hard_link guarantees identical-to-source sizes. Drop or `debug!`.

9. **`restore_handler.rs:1191-1208` — fresh `ureq::Agent` per `nomad_get/post_blocking` call.** 4 polls/s × 120s = 480 TCP handshakes per cold-boot against localhost Nomad. Shared `OnceLock<Agent>` collapses this; small (~50-100ms) but free.

## B20 root-cause (perf lens)

Deferred-file's 4 hypotheses (ch-remote/CH/rootfs/init.sh/vmlinuz drift) are non-perf. From perf, finding #3 is the most likely contributor: cold-cache worker doing 1 GB rootfs `cp` + 20 GB `mkfs.ext4` per-create can stretch wrapper-to-CH-spawn from ~250ms to seconds, eating the 30s budget. Tap `<NO-CARRIER>` matches "CH never reached kernel net init" (CH not yet spawned). **First step (per deferred)**: pull `/var/log/zeroship-sandbox.log` + `ch.stderr.0` + `ls -la /opt/sandbox-artifacts/rootfs-slim.img` off worker; reproduce locally.

## A2b verify status

Confirmed open: cold-boot does NOT call `verify` (`restore_handler.rs:365` uses `get`). Closure of A2b doesn't move cold-boot or wake p50 — structural debt for the future periodic-L2-sweep wiring.

## Next A3 slice (recommended)

**`BufReader::with_capacity(1 << 20, f)` at `snapshot_store.rs:168` + wrap `store.get` in `compio::runtime::spawn_blocking` at `restore_handler.rs:365`.** Two-line change: SHA syscall count drops 16×, throughput moves syscall→memory-bandwidth (~1 GB/s), and the SHA leaves the ntex worker. Expected wake p50 reduction (unmeasured): ~1.5-2.5s on c=1, ~3-5s on c=4 (eliminates serial single-core SHA contention). Combined with existing hard_link, c=1 wake budget drops to ~3-5s. Third A3 slice (after this): swap `std::thread::sleep` → `compio::time::sleep` at `:1302,1335`.

## Two most critical citations

- `restore_handler.rs:327-496,1302,1335` — A3 unmoved structurally; sleeps + sync I/O still park ntex worker. Hard_link closes the copy slice only.
- `snapshot_store.rs:153-185` — `compute_artifact_sha256` 64 KiB unbuffered SHA is now the largest single-line wake-cost lever. Single-line `BufReader` + one `spawn_blocking` wrap = highest-ROI next A3 slice.
