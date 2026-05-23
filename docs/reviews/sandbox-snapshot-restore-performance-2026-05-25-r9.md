# Performance review — 2026-05-25 round 9

**HEAD**: `dfd1a43d`. Static review only; no fresh cluster numbers.
**Baseline (r8 / Appendix F c=4)**: snapshot p50 50573 ms, wake p50 9235 ms.

## Summary

**8 findings** (2 CRITICAL, 4 IMPORTANT, 2 MINOR). A3 slices 1-5 closed; r8's #1 (sync `submit_restore_job`+`wait_for_livez`) is gone. The new wake-path long-pole is the **AEAD-active get path** which discards r5's hard-link zero-copy (~1 GB extra write+copy). Snapshot p50 dominated by SSD bandwidth contention; AEAD-active put adds ~1.5–2 s but doesn't change the ordering.

## Estimated NEW wake p50 baseline (post-A3 full)

| Component | Estimate | Source |
|---|---|---|
| `store.get` (1 GB SHA + hard-link, no AEAD) | ~1.5–2.0 s | r8 §"Post-A3-full" |
| Slice-5 win (`submit_restore_job` + `wait_for_livez` off ntex worker) | -5–8 s | `restore_handler.rs:484-518` |
| Nomad submit RTT + first /livez 200 | ~1.0–2.3 s | `restore_handler.rs:1398-1485` |
| `clock_resync` (already spawn_blocking) | ~0.1–0.2 s | `restore_handler.rs:1547` |
| pg awaits + state-map | ~0.05–0.1 s | r8 §"Post-A3-full" |

**Estimated wake p50 (AEAD inactive, hard-link path)**: **~2.5–4.5 s** (down from 9.2 s).
**Estimated wake p50 (AEAD active, copy path)**: **~5.5–7.5 s** (AEAD decrypt + 1 GB extra write dominates; see finding #1).

## CRITICAL

1. **`snapshot_aead.rs:619-664` — AEAD-active `get` discards the R5-P1 hard-link zero-copy win.** The wrapper writes ciphertext to a sibling `stage/` (inner.get hard-links from L1) **and then `decrypt_to` writes a full plaintext copy** to `target_dir/memory-ranges`. On a 1 GB artifact this is +1 GB sequential write + ChaCha20 decrypt (~0.66 s on ~1.5 GB/s/core) on the wake path. The L1 hard-link the R5-P1/R5-S5 fast-path created in `stage` is dropped seconds later. **At c=4 the four concurrent ~1 GB writes contend on the SSD queue — this is the biggest new wake-path target.** Mitigation options: (a) decrypt in place into the alloc dir without the `stage/` intermediate (rename ciphertext into alloc, decrypt to a `.plain.tmp`, rename), (b) parallel decrypt+write using `std::os::unix::fs::FileExt::write_at` from multiple threads. Either keeps to one 1 GB write rather than the current two-write shape after taking the inner hard-link copy of memory-ranges into account.

2. **`snapshot_aead.rs:363-451` — AEAD `encrypt_in_place` triples I/O passes on snapshot.** Order: (1) `encrypt_in_place` reads plaintext from `source_dir/memory-ranges` (1 GB) + writes ciphertext to `source_dir/memory-ranges.aead.tmp` (1 GB) + rename; (2) `inner.put` calls `compute_artifact_sha256` which **reads the ciphertext again** (1 GB) for the SHA; (3) `rename` into L1. That's 2× full reads + 1 full write of memory-ranges per snapshot. The encrypt + SHA passes are fusable: stream plaintext through the ChaCha20 sealer and a `Sha256` hasher in the same loop, write ciphertext once, return the hash to skip step (2) entirely. **Estimated saving: ~0.6–1.0 s per snapshot @ ~1.5–2 GB/s SSD read.** Stacks with finding #5.

## IMPORTANT

3. **`restore_handler.rs:1340 + :1350 + :1582` — no persistent `ureq::Agent`.** Every poll in `wait_for_alloc_running_blocking` (250 ms cadence) and `wait_for_livez_blocking` (150 ms cadence) opens a fresh TCP connection (`ureq::get(url).timeout(...)` constructs a one-shot agent per call). At c=4 typical wake of ~3 s livez wait → ~20 connections per wake; clock_resync adds one more. A single `ureq::Agent::new()` cached in a `OnceLock` would keep-alive these to the same nomad/agent endpoint and save ~100–300 ms wake p50. Stacks on top of slice-5.

4. **`snapshot_store_gcs.rs:1166-1183` + `sweep.rs` — A2b fast-path plumbed but unused.** `TieredSnapshotStore::verify_metadata_only` delegates correctly to the GCS HEAD path (`snapshot_store_gcs.rs:693-719`), but `Grep '\.verify(_metadata_only)?\('` over `crates/sandbox/src/sweep.rs` returns **zero hits** — the periodic sweep never calls either. The brief states "fast-path is the default for periodic sweeps"; **it is not, because no sweep verifies snapshots today**. A2b is dead code in production until a sweep callsite lands. Action: confirm whether an integrity-sweep is in scope this cycle; if yes, wire `state.snapshot_store.verify_metadata_only(...)` into a sweep loop (cadence ~24h).

5. **`snapshot_store.rs:184-223` — `compute_artifact_sha256` is sequential over 3 files.** R8 already flagged the parallel-SHA win. The 1 GB `memory-ranges` SHA dominates (~1.4 s @ ~700 MB/s on a single core); `config.json` (~2 KB) and `state.json` (~110 KB) are noise. Splitting only matters if we want to overlap **SHA-compute with disk read** (already overlapped by `BufReader` prefetch in practice). A more meaningful win is to **fuse SHA with the AEAD encrypt pass** (finding #2) and **with the I/O pipe on `inner.put`-side rename for AEAD-passthrough mode**.

6. **`snapshot_store.rs:206-207` — 64 KiB scratch buffer allocated per call inside the `for &name in ARTIFACT_FILES` loop.** Three allocations per SHA per snapshot/restore. Hoist outside the loop or use a stack array `[0u8; 64 * 1024]`. Same target as r8 finding #8 — still present at `:207`. Trivial fix.

## MINOR

7. **`restore_handler.rs:1632-1640` — `clock_resync_random_hex` still uses 48× `format!("{:02x}")` (r8 finding #3 unaddressed).** ~80–120 µs per resync. Negligible vs RTT but trivial to swap to a 16-byte hex lookup table.

8. **`snapshot_aead.rs:741-752` — `chunk_aad()` allocates a 13-byte `Vec` per chunk.** 1024 chunks per 1 GB snapshot → 1024 small allocs on both put and get paths. Use a `[u8; 13]` stack array. Sub-ms aggregate; only worth doing in the same edit pass as finding #2.

## Ranked next-biggest perf lever

1. **Fuse AEAD-encrypt + SHA-256 into one pass on snapshot put** (finding #2): ~0.6–1.0 s/snapshot saved; eliminates one full SSD read of memory-ranges. Also relieves c=4 SSD-queue contention.
2. **Eliminate the second 1 GB write on AEAD-active wake** (finding #1): ~0.5–1.5 s/wake saved; restores the R5-P1 hard-link advantage to the AEAD-active deploy posture.
3. **Persistent `ureq::Agent`** (finding #3): ~100–300 ms/wake. Smallest code change; lowest impact.

## Two most critical citations

- `snapshot_aead.rs:619-664` — AEAD-active wake re-copies the entire 1 GB plaintext into `target_dir`, **erasing R5-P1's hard-link zero-copy win** the moment AEAD is enabled in prod. The biggest new wake-path target.
- `snapshot_aead.rs:363-451` + `snapshot_store.rs:184-223` — AEAD encrypt + canonical SHA each stream the full 1 GB sequentially in separate passes; fusing them halves the snapshot-side memory-ranges I/O.
