# Performance review — 2026-05-24 round 2

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: 3b888a2a
**Lens**: performance
**Last reviewed (this lens)**: 2026-05-24 r1 (`sandbox-snapshot-restore-performance-2026-05-24-r1.md`)

## Summary
- 8 findings (2 CRITICAL, 4 IMPORTANT, 2 MINOR).
- **r1 closures**: none of r1's 10 findings has been fixed yet — A3 still open (sync I/O on compio worker), GCS-put still triple-reads (r1 CRITICAL #3), 22-knob magic-number poll still open, AEAD per-chunk `Vec` still open. The r2 commits closed A2 (GCS verify enforcement, security lens) and A5 (admin_token field visibility, api-surface lens) but neither touched the perf axis.
- **New r2 hot-spots**: (a) `GcsSnapshotStore::verify` now re-streams every artifact (full 1 GB) on each invocation; (b) `ControllerIdleSnapshotter` queues N sequential 2.1 s snapshots inside one compio task with no parallelism; (c) restore `do_restore_inner` is still entirely serial on a single compio worker.

## CRITICAL

**`crates/sandbox/src/snapshot_store_gcs.rs:586-611` (verify) + `crates/sandbox/src/snapshot_store_gcs.rs:920-930` (`TieredSnapshotStore::verify`) — verify re-streams ~1 GB from GCS unconditionally, with no caller-side caching.**
The A2 fix at `f32507ce` replaced the no-op verify with a full re-stream (lines 608-610 call `open_object_stream` per file). Tiered verify (line 925) prefers L1, falls back to L2 on `NotFound` — but the moment the L1 disk is evicted (e.g. after a controller restart / disk-pressure prune) every verify call pulls a full 1 GB egress from GCS through a single-threaded SHA-256. No callers in tree today, but as soon as a periodic-L2-sweep wires up, N opted-in sandboxes × ~1 GB / 5-min interval becomes 200+ MB/s sustained GCS egress + 100 % of one core. The module doc on line 605-607 warns "callers should reach for `verify` sparingly" — but the trait gives no hint of that, and the helper makes the expensive path the default. Mitigation: gate verify behind a `last_verified_at` column on `snapshot_metadata` + a sample rate; OR store the per-file SHA in pg so verify can be a single HEAD-and-compare against `x-goog-hash` for the no-attacker case.

**`crates/sandbox/src/sweep.rs:443-468` + `crates/sandbox/src/sweep.rs:302-382` — idle sweep awaits each snapshot serially inside `run_idle_eviction_once`, AND each `snapshot_one` is itself sync I/O on the compio worker (r1 [A3]).**
T6 wired `ControllerIdleSnapshotter` (`4a7e8e03`), but `run_idle_eviction_once` (line 449) is `for r in chunk { snapshotter.snapshot_one(sid).await }` — no `join_all`, no spawn. With `IDLE_BATCH_LIMIT=100` (line 77) and `per_iteration_concurrency=2` (line 63), at 2.1 s/snapshot the per-iteration cost is 100 × 2.1 s = 210 s — exactly the comment's "no contention" claim, but it consumes the *entire 300 s sweep budget* in worst case. Worse, every `snapshot_one.await` chains `lookup_source_vm_ops` → `snapshot_handler::snapshot_sandbox` (sync SHA + AEAD + pg trio per r1 A3) — all on the single compio worker that also serves the live HTTP surface. A burst of N idle snapshots = N × ~2.1 s of worker stall back-to-back; the per_iteration_concurrency name is pure documentation. Fix: `futures_util::future::join_all(chunk.iter().map(|r| spawn_blocking(...)))` AND restore the r1 [A3] `spawn_blocking` around the snapshot_handler call.

## IMPORTANT

**`crates/sandbox/src/snapshot_store_gcs.rs:493-543` — GCS `put` is unchanged from r1: still 3 reads + 2 SHA passes per artifact.**
Lines 504-523 walk `ARTIFACT_FILES`: `metadata` → `sha256_file` (read pass 1, hash 1, 64 KiB chunks at line 462) → `upload_*` (read pass 2, lines 305-322). Then line 527 calls `canonical_artifact_sha256` (re-opens all 3 files, read pass 3, hash 2). On a 1 GB memory-ranges file: ~3 GB disk read + ~2 GB hashed = ~6 s of single-core work. r1 CRITICAL #3 already named this; no progress. This is the single biggest snapshot-path win available.

**`crates/sandbox/src/restore_handler.rs:286-412` — restore still entirely serial on the compio worker.**
`do_restore_inner` is the same shape as r1: `remove_dir_all` → `create_dir_all` → `store.get` (sync, full 1 GB SHA + AEAD decrypt) → `rewrite_config_json` → `backend.submit_restore_job` → `backend.wait_for_livez` (which contains `std::thread::sleep` at lines 1088, 1121). The two-pass stat block at lines 331-345 (diagnostic logging) opens each file a fourth time after `store.get` already touched them — small per-file but compounds the issue. r1 A3 unfixed; nothing in r2 changed this path.

**`crates/sandbox/src/snapshot_store.rs:153-185`, `snapshot_store_gcs.rs:459-474`, `snapshot_aead.rs:380-451` — every streaming read path uses raw `File::read` with a 64 KiB or 1 MiB buffer; no `BufReader`/`BufWriter`.**
Five places (compute_artifact_sha256, sha256_file, encrypt_in_place, decrypt_to, verify_canonical_sha256_from_streams) open `std::fs::File` directly and call `f.read(&mut buf)` in a loop. On ext4 with read-ahead this is fine for sequential streaming, but the `dst.write_all(&chunk_len.to_be_bytes())` then `dst.write_all(&ciphertext)` pair in `encrypt_in_place` (lines 431-432) does two unbuffered write syscalls per 1 MiB chunk = ~2000 extra syscalls per 1 GB encrypt. `BufWriter::with_capacity(1 << 20, dst)` collapses these. Similarly `verify_canonical_sha256_from_streams` (line 633) opens a 64 KiB read buffer against `Box<dyn Read + Send>` ureq stream — wrapping the inner reader in `BufReader::with_capacity(1 << 20, ...)` reduces TCP-recv syscalls for the 1 GB pull.

**`crates/sandbox/src/snapshot_store_gcs.rs:546-564` — GCS `get` still serializes the 3 file downloads.**
Same as r1 MAJOR #1: `for &name in ARTIFACT_FILES { download_to_disk(...) }` (line 552). The two small files (config.json ≈ 3 KB, state.json ≈ 100 KB) sit in line behind memory-ranges (~1 GB). They are independent — at minimum `compio::runtime::spawn` each, await all three. Adds ~0 risk; the post-download canonical-hash call (line 556) is the natural join point.

## MINOR

**`crates/sandbox/src/snapshot_handler.rs:316-373` + `crates/sandbox/src/sweep.rs:296-300` — `ControllerIdleSnapshotter::new(state.clone())` clones the `Arc<AppState>` once at sweep spawn; no hot-path clone cost.**
A5 builder pattern (`with_admin_token` at `lib.rs:173-190`) takes `self` by value — `mut self` semantics, no clone introduced. Production wiring (`lib.rs:445-457`) constructs once. The r2 builder adds zero hot-path cost. Confirms the question raised in the request.

**`crates/sandbox/src/snapshot_aead.rs:311-316` — `chunk_aad` still allocates a 13-byte `Vec` per 1 MiB chunk (~1000 allocs per 1 GB encrypt).**
Carried from r1 MINOR #1; no progress. Stack-allocate `[u8; 13]` and pass by reference to `Payload { aad: &arr }`. The cost is dwarfed by the AEAD itself but it's the cheapest available win.

## Two most critical citations
- `crates/sandbox/src/sweep.rs:443-468` + `:302-382` — idle sweep serial + sync I/O on compio worker (N × 2.1 s back-to-back stall)
- `crates/sandbox/src/snapshot_store_gcs.rs:586-611,920-930` — verify re-streams full ~1 GB on every call; tiered verify falls through to GCS the moment L1 is evicted

## r1 closure status
- r1 A2 (security lens): closed at `f32507ce`. **Side effect**: created the new CRITICAL in this review.
- r1 A5 (api-surface lens): closed at `2e0d17f7`. **Performance impact**: none (verified above).
- r1 CRITICAL #1 (sync SHA+AEAD on compio worker): open ([A3] backlog).
- r1 CRITICAL #2 (restore sync I/O + `std::thread::sleep`): open ([A3] backlog).
- r1 CRITICAL #3 (GCS put 3 reads + 2 SHA): open.
- r1 MAJOR #1-4: all open.
- r1 MINOR #1-3: all open.
