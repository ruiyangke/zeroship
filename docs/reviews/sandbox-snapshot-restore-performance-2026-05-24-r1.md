# Performance review — 2026-05-24 round 1

**Reviewer**: pilot-cron Part 1 (read-only)
**Worktree HEAD**: b048b491
**Lens**: performance
**Last reviewed (this lens)**: zero prior reviews

## Summary
- 10 findings (3 critical, 4 major, 3 minor)
- Themes: (1) **restore path serializes three SHA-256 passes over the 1 GB memory-ranges file** before CH even sees the bytes — L1+AEAD+GCS each rescan the file; (2) **`SnapshotStore::put`/`get` execute on the compio worker without `spawn_blocking`** in `snapshot_handler`/`restore_handler`, stalling a worker for the entire ~1 GB SHA + AEAD + GCS roundtrip; (3) **knob-less poll loops** scatter `Duration::from_secs(5/10/15/30)` across `nomad_ch.rs` (22 hits) and `restore_handler.rs`; (4) GCS `put` reads the file twice (once for SHA, once for the stream) and `get` lacks any retry; (5) hot-path `format!` / `.clone()` / `Vec` reallocations are pervasive but each is small — the I/O wins dominate.

## CRITICAL

**`crates/sandbox/src/snapshot_handler.rs:316-373` — synchronous SHA-256 + AEAD + rename run on the compio worker, no `spawn_blocking`**
  Why: `do_snapshot_inner` is `async` but calls `store.put(...)` (line 346) directly. With `AeadSnapshotStore` wrapping `LocalDiskSnapshotStore`, the call chain encrypts ~1 GB (chacha20+poly1305, 1 MiB chunks, single-threaded — `snapshot_aead.rs:399-439`), then `compute_artifact_sha256` (`snapshot_store.rs:153-185`) hashes the now-ciphertext memory-ranges over a 64 KiB buffer in a tight loop, then `fs::rename` per file. Module headers on lines 95-96 / 378-380 explicitly say "async callers should `spawn_blocking`" — but the handler ignores its own contract. On a 1 GB artifact this blocks the worker thread for hundreds of ms (SHA-256 single-thread ≈ ~500 MB/s on n2-standard). Every concurrent request multiplexed on that worker stalls.
  Fix: wrap the body of `do_snapshot_inner` (or at minimum `ch.snapshot` + `store.put`) in `compio::runtime::spawn_blocking` — same pattern as `persist.rs:641`.

**`crates/sandbox/src/restore_handler.rs:286-412` — restore body identical: `store.get` (SHA + AEAD-decrypt of 1 GB) runs on the compio worker**
  Why: `do_restore_inner` is `async` but `store.get` at line 323 is synchronous. With AEAD enabled, `get` runs `LocalDisk::get` (SHA over 1 GB ciphertext — `snapshot_store.rs:239`) THEN `AeadSnapshotStore::decrypt_to` (1 GB chacha20 stream — `snapshot_aead.rs:518-560`). Two full passes over the file, both single-threaded, both on the worker. Then `wait_for_livez` at line 388 chains a blocking `ureq::get` loop (`nomad_post_blocking`/`nomad_get_blocking` — `restore_handler.rs:977-1034`) that hard-`std::thread::sleep(...)`s the compio worker (lines 1088, 1121). One in-flight restore parks the worker for the entire artifact roundtrip + CH boot.
  Fix: `spawn_blocking` the whole `do_restore_inner` closure; replace the `std::thread::sleep` in `wait_for_alloc_running_blocking` / `wait_for_livez_blocking` with `compio::time::sleep` (or move the polling out of the blocking pool entirely).

**`crates/sandbox/src/snapshot_store_gcs.rs:566-617` — GCS `put` hashes each file twice and reads each file twice**
  Why: For every artifact file the loop calls `sha256_file(&path)` (lines 588 / 533-548, full streaming pass) THEN `upload_resumable(...)` (line 593) which opens the SAME file and streams it again (lines 304-322) for the upload. Then `canonical_artifact_sha256(source_dir)` (line 601 / 689-720) opens AND re-hashes ALL THREE files a third time to compute the canonical artifact SHA. That is 3 full reads + 2 full SHA-256 passes over the 1 GB memory-ranges per `put`. With AEAD the wrapped blob is what's being re-read each pass — pure waste.
  Fix: hash-while-streaming the upload path (interleave `hasher.update(chunk)` inside the chunk loop at `snapshot_store_gcs.rs:307-360`), and reuse the per-file hash to compose the canonical hash without re-reading. Net win: ~2× wall time on a 1 GB artifact.

## MAJOR

**`crates/sandbox/src/snapshot_store_gcs.rs:619-638` — GCS `get` has no retry and downloads files serially**
  Why: `download_to_disk` does a single `ureq::get(...).call()` per file, with no exponential-backoff retry; a 503/transient TCP drop on byte 800M of memory-ranges restarts the *entire 1 GB* from byte 0 (no Range header), pinning restore wall time to "best of 1 attempt". Then the three files download serially (`for &name in ARTIFACT_FILES`) — `config.json` (≪1 MB) and `state.json` (~110 KB) blocking on the `memory-ranges` GET. Add retry with `Range: bytes=<resume>-`; parallelize the three downloads on `compio::runtime::spawn` since they're independent.
  Fix: retry with backoff (cap ~5 attempts), use a `Range` resume on partial failures, dispatch all three downloads concurrently and await all.

**`crates/sandbox/src/snapshot_handler.rs:329 / restore_handler.rs:301-313` — staged-dir setup uses sync `std::fs` on the async path**
  Why: `create_dir_all` + `remove_dir_all` are invoked directly on the compio worker. For a stale 1 GB `alloc_dir` (`restore_handler.rs:303`), `remove_dir_all` walks the directory + issues per-file `unlink` syscalls inline — observable hundreds of ms when the FS is busy.
  Fix: also wrap in `spawn_blocking`. Same pattern as `persist.rs`.

**`crates/sandbox/src/backend/nomad_ch.rs` + `restore_handler.rs` — 22 instances of `Duration::from_secs(5/10/15/30)` magic-number poll/timeout knobs**
  Why: Grep shows literal `Duration::from_secs(5)` 11×, `from_secs(10)` 4×, `from_secs(15)` 2×, `from_secs(30)` 2× across `nomad_ch.rs`. `restore_handler.rs` mirrors three of them inline (lines 789, 823, 1050). No env-overridable knob; operators tuning fleet latency for a slow-host cohort must recompile.
  Fix: hoist into `NomadCHConfig` (alloc_poll_interval_ms, livez_poll_interval_ms, http_call_timeout_secs, teardown_delete_timeout_secs) and read once at backend construct. Wire one knob, not 22 literals.

**`crates/sandbox/src/snapshot_store.rs:153-185` / `snapshot_store_gcs.rs:689-720` / `snapshot_aead.rs:399-439` — SHA-256 and AEAD are single-threaded with 64 KiB / 1 MiB chunks**
  Why: All three streaming hashers/encryptors are a `loop { read; hasher.update }` on one thread. A 1 GB pass at ~500 MB/s SHA-256 ≈ 2 s wall; with three serialized passes (L1 verify + AEAD decrypt + canonical-hash) that's 6 s of CPU sitting on one core. Rayon-parallel chunked SHA (4× speedup on 4 cores is routine) or interleaving SHA with the I/O (overlap disk read with prior chunk's hash) reduces wall time.
  Fix: at minimum overlap I/O with hashing (double-buffer the read); ideal is hash-while-encrypting in one pass with rayon-fanout for the canonical hash post-AEAD.

**`crates/sandbox/src/backend/nomad_ch.rs:920-1668` — `stop_inner` (lines 920-1170+) and `lookup_source_vm_ops` (1575-1668) are long, monolithic functions that block tooling-level optimization**
  Why: `stop_inner` reads as 5 sequential phases (drain → purge → wait-job-gone → host-fence → host_dir rm). Each phase is awaited serially even when independent (e.g., the in-memory `state.remove` at line 928 could be done concurrently with the `/shutdown` HTTP). `lookup_source_vm_ops` runs `state.read` → `http_get_unsigned` → `serde_json::from_str` → `fs::metadata` serially. Hard to see where to parallelize without breaking the function up.
  Fix: extract the 5 stop-phases into named async helpers, then audit which can be issued concurrently with `futures::join!`. Same for `lookup_source_vm_ops`'s alloc lookup + socket stat.

## MINOR

**`crates/sandbox/src/snapshot_aead.rs:311-316 / 372-282` — per-chunk `Vec` alloc + sandbox_id `Vec::with_capacity` in tight loops**
  Why: `chunk_aad(idx)` allocates a fresh 13-byte `Vec` on EVERY 1 MiB chunk (≈1000 allocs per 1 GB encrypt). `derive_dek` (line 275) allocates a fresh salt `Vec` per snapshot — fine — but `chunk_aad` is on the hot loop. Could be `[u8; 13]` returned by-value, or a single reusable buffer threaded through the chunk loop.
  Fix: stack-allocate; the AAD format is fixed-size.

**`crates/sandbox/src/restore_handler.rs:316-319 / snapshot_handler.rs:341-344 / admin_handlers.rs:1208-1211` — `format!("sbx_{}", uuid_to_base62(&id))` recomputed at every callsite**
  Why: The typed-id string is recomputed from the same UUID in multiple places per request (restore handler at lines 159, 252, 317; snapshot handler at 239, 342; admin handler at 1209, 1270). Each call is a base62 encode + a `format!` heap-allocation. Compute once at the handler entry and pass `&str` down.
  Fix: build the typed string once per request, propagate by reference.

**`crates/sandbox/src/restore_handler.rs:444-475` — `rewrite_config_json` round-trips through `serde_json::Value`**
  Why: `from_str` → mutate `Value` → `to_string_pretty` allocates the entire config tree twice for an in-place edit of two fields (`net[].tap`, `net[].mac`). For the v1 single-vm-index path the file is ~2.4 KB so the cost is small, but the proposal mentions v2 cluster-fallback where this would run on every restore. A two-line `regex_replace`-style edit or `jsonpatch` would skip the allocation churn.
  Fix: defer until v2 cross-cluster lands; track as a known-cost item.

## Two most critical citations
- `crates/sandbox/src/snapshot_handler.rs:316-373` (sync SHA + AEAD + rename on compio worker, no `spawn_blocking`)
- `crates/sandbox/src/restore_handler.rs:286-412` + `1088,1121` (sync SHA + AEAD decrypt of 1 GB AND `std::thread::sleep` parking the compio worker)
