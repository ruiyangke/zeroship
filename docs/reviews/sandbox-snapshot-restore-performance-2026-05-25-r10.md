# Sandbox/snapshot-restore — performance r10 review

Date: 2026-05-25 (UTC)
HEAD at audit: `e7b3278b`
Round 10 of N.

Static review only; no fresh cluster numbers. Baseline (r8 / Appendix F
c=4): snapshot p50 50573 ms, wake p50 9235 ms.

## Summary

**7 findings** (2 CRITICAL, 3 IMPORTANT, 2 MINOR). All R9 carry-forward
items are still open. The biggest **new** finding is the AEAD-active
snapshot-put I/O bill on the **L1+L2 tiered composition**: the wrapper
stack reads `memory-ranges` ~5 times sequentially per snapshot when AEAD
+ GCS are both enabled (R10-P1). The other new finding is R7-P2 hard
evidence: `TieredSnapshotStore::put` spawns L2 GCS upload via
`compio::runtime::spawn(async move { l2.put(...) })` — sync HTTP I/O on
a compio task thread, NOT `spawn_blocking` (R10-P2).

## Findings (NEW since r9)

### [R10-P1] AEAD-active snapshot put reads memory-ranges 5× sequentially when both AEAD and GCS are enabled (CRITICAL, performance-r10)

- **Files**:
  - `crates/sandbox/src/snapshot_aead.rs:363-449` (encrypt_in_place — 1 read + 1 write)
  - `crates/sandbox/src/snapshot_store.rs:184-223` (LocalDisk::put → compute_artifact_sha256 — 1 read)
  - `crates/sandbox/src/snapshot_store_gcs.rs:559-560` (canonical_artifact_sha256 — 1 read)
  - `crates/sandbox/src/snapshot_store_gcs.rs:572` (sha256_file per file — 1 more read of memory-ranges)
  - `crates/sandbox/src/snapshot_store_gcs.rs:354-411` (upload_resumable streams from disk — 1 read)
- **Symptom**: Production composition is
  `AeadSnapshotStore<TieredSnapshotStore<LocalDisk, Gcs>>` (per
  `lib.rs:640-666`). A single snapshot of a 1 GB `memory-ranges` walks
  the disk sequentially:
  1. `encrypt_in_place` reads plaintext + writes ciphertext (1 R + 1 W
     of 1 GB).
  2. `TieredSnapshotStore::put` calls `l1.put` →
     `compute_artifact_sha256` reads the (now ciphertext) memory-ranges
     (1 R).
  3. `l2.put` (GCS) is spawned. It calls `canonical_artifact_sha256`
     reading memory-ranges again (1 R).
  4. The same loop then calls `sha256_file(&path)` on memory-ranges (1
     more R) to compute the per-file `x-goog-hash`.
  5. `upload_resumable` re-opens memory-ranges and streams it to GCS (1
     R).
  Total: **5 sequential reads + 1 write of 1 GB per snapshot** =
  ~6 GB local disk I/O for a 1 GB guest. Plus 1 GB GCS egress (the
  upload itself).
- **Hot path**: Every AEAD-active snapshot under the prod GCS-on
  config. The idle-eviction sweep at `SANDBOX_SNAPSHOT_PER_WORKER_
  CONCURRENCY=2` does up to 2 of these in parallel; at c=4 stress the
  four concurrent snapshots collectively walk the SSD ~20× sequentially
  across `memory-ranges`.
- **Estimated p50 delta**: at SSD sequential read ~1.5 GB/s/single-
  stream on n2-standard-32, the avoidable passes are 3 of the 5 reads
  (encrypt+upload reads remain; the three SHA passes fuse into the
  encrypt + upload streams). **Estimated saving: 3 × ~0.66 s ≈ 1.5–2.5
  s per snapshot.** Unknown — depends on SSD bandwidth at c=N
  concurrent dumps, where contention is non-linear past 2 streams.
- **Action**:
  1. Fuse encrypt + canonical-SHA into one streaming pass (r9 #2 —
     still open; carries forward into this finding).
  2. In `TieredSnapshotStore::put`, plumb the L1 metadata's
     `sha256` + per-file bytes through to the spawned L2 task so L2
     skips its own canonical SHA. (The hash is already known.)
  3. In `GcsSnapshotStore::put`, fuse per-file SHA with the
     upload pipe — a `BufReader` that updates `Sha256` while feeding
     `send_bytes` / chunked PUT writes. Eliminates the
     `sha256_file(&path)` pass.

### [R10-P2] `TieredSnapshotStore::put` spawns blocking GCS HTTP via `compio::runtime::spawn` instead of `spawn_blocking` (CRITICAL, performance-r10; closes R7-P2 as confirmed-open)

- **Files**: `crates/sandbox/src/snapshot_store_gcs.rs:1066-1093`
- **Symptom**: The fire-and-forget L2 upload is:
  ```rust
  compio::runtime::spawn(async move {
      // Synchronous I/O inside the task — the stub returns
      // immediately. When real GCS lands, wrap in
      // spawn_blocking like the rest of the controller.
      match l2.put(&sandbox_id, &artifact_path, &ch_version_owned) { ... }
  }).detach();
  ```
  The TODO comment ("wrap in spawn_blocking like the rest of the
  controller") was never executed. `GcsSnapshotStore::put` is fully
  synchronous (`ureq` POST / PUT, blocking file I/O) and now runs on a
  compio executor thread. While the L2 put runs (~10–30 s for 1 GB at
  ~30–80 MB/s GCS) **the compio thread that picked up this task cannot
  drive any async I/O** — including epoll wakeups for `/exec`, `/livez`
  polls, db futures, or other snapshots/restores.
- **Hot path**: every successful snapshot. Detached, but the executor
  thread is parked the entire upload window — which on the same
  controller is the same window during which c=20 wake stress would
  want async progress.
- **Estimated p50 delta**: tail wake latency under load: unknown —
  depends on (a) how many compio executor threads are configured, (b)
  whether a wake-path future lands on the parked thread. With `N`
  executor threads and `M` concurrent snapshots-in-L2-upload, the
  probability a given wake stalls behind one of these is `M/N` per
  scheduler tick. At the default executor sizing (typically
  `num_cpus`) and c=20 stress where M can be 2–4 concurrent, ~10–20%
  of wakes could hit a parked thread. **Median wake unaffected;
  p95–p99 wake could pick up multi-second tail.**
- **Action**: Wrap the inner `l2.put(...)` call in
  `compio::runtime::spawn_blocking(move || ...)` inside the spawned
  task and `.await` it. The `spawn` outer wrapper is correct
  (fire-and-forget); only the inner sync I/O needs the blocking-pool
  hop. Two-line diff.

### [R10-P3] `cipher.encrypt`/`cipher.decrypt` allocate a fresh `Vec<u8>` per 1 MiB chunk → 1024 heap allocations per 1 GB encrypt-pass (IMPORTANT, performance-r10)

- **Files**:
  - `crates/sandbox/src/snapshot_aead.rs:415-425` (encrypt)
  - `crates/sandbox/src/snapshot_aead.rs:539-556` (decrypt)
- **Symptom**: `ChaCha20Poly1305::encrypt(nonce, Payload)` returns a
  freshly-allocated `Vec<u8>` of size `CHUNK_PLAINTEXT_LEN +
  AEAD_TAG_LEN` ≈ 1 MiB + 16. On a 1 GB `memory-ranges` that's 1024
  Vec allocations on snapshot-put + 1024 on restore-get. Each allocation
  is a 1 MiB+16 jemalloc/mmap dance; the OS is being asked for fresh
  anonymous pages for every chunk and the allocator must zero or
  scrub them.
- **Hot path**: every AEAD-active snapshot + every AEAD-active wake.
- **Estimated p50 delta**: unknown — depends on allocator and page-
  fault rate at 1 MiB allocation granularity. On glibc malloc 1 MiB
  allocations go through `mmap` rather than the small-bin freelist, so
  each is a page-fault round-trip. **Plausible 50–200 ms per pass;
  unknown without measurement.** Smaller win than R10-P1 but stacks
  with it (same fused-encrypt-pass code edit).
- **Action**: Use `aead::AeadInPlace::encrypt_in_place_detached` /
  `decrypt_in_place_detached`. The plaintext buffer `buf` (already
  allocated at line 397 / `ct_buf` at line 517) is reused in place; the
  16-byte tag returns as a stack `[u8; 16]`. One-pass write to `dst`:
  `dst.write_all(&buf[..filled])` + `dst.write_all(&tag)` (plus the
  length prefix). No per-chunk heap allocations.

### [R10-P4] `GcsSnapshotStore::put` recomputes the canonical SHA-256 even though `TieredSnapshotStore::put` already knows it (IMPORTANT, performance-r10)

- **Files**:
  - `crates/sandbox/src/snapshot_store_gcs.rs:539-622` (`put`)
  - `crates/sandbox/src/snapshot_store_gcs.rs:559-560` (canonical_artifact_sha256 redundant call)
  - `crates/sandbox/src/snapshot_store_gcs.rs:1048` (TieredSnapshotStore::put — meta.sha256 in hand)
- **Symptom**: `TieredSnapshotStore::put` has `meta` (with
  `meta.sha256` computed by L1) BEFORE it spawns L2.put. The
  `SnapshotStore::put` trait signature doesn't accept an
  already-computed canonical SHA; `GcsSnapshotStore::put`
  re-computes it from disk (the call at line 559–560). One full
  re-read of 1 GB memory-ranges that the L1 put already paid for.
- **Hot path**: every snapshot when GCS is enabled. Fires regardless
  of AEAD posture.
- **Estimated p50 delta**: ~0.5–1.0 s/snapshot (one full sequential
  read of memory-ranges, single-streamed). Stacks on R10-P1 (encrypt
  + canonical-SHA fusion would eliminate the L1-side pass; this
  finding eliminates the L2-side pass). Together the two could fuse
  the encrypt-time read into the upload pipe and skip the two
  intermediate SHA reads entirely — net `memory-ranges` traffic on
  snapshot drops from 5 R + 1 W to 1 R + 1 W (encrypt) + 1 R (GCS
  upload) = 2 R + 1 W. **Down from ~6 GB to ~3 GB of local disk I/O
  per 1 GB snapshot.**
- **Action**: Extend `SnapshotStore::put` to accept an optional
  pre-computed canonical SHA-256 (or add a separate
  `put_with_known_sha256` method). When the tiered store spawns the
  L2 put it passes `meta.sha256`; the GCS impl skips the
  `canonical_artifact_sha256` precomputation and only retains the
  per-file SHA pass (which it still needs for `x-goog-hash`). Or,
  more aggressively: keep the canonical SHA in the GCS object's
  custom metadata (already done — `CANONICAL_SHA_METADATA_KEY`) and
  trust L1 for the input hash.

### [R10-P5] AEAD encrypt + decrypt iterate without a `BufWriter`/`BufReader` on the output streams (MINOR, performance-r10)

- **Files**:
  - `crates/sandbox/src/snapshot_aead.rs:389` (`dst = File::create(&temp_path)`, encrypt)
  - `crates/sandbox/src/snapshot_aead.rs:515` (`dst = File::create(target_path)`, decrypt)
- **Symptom**: Both encrypt and decrypt write to a raw `std::fs::File`
  with no `BufWriter`. The chunk-write loop emits two `write_all`
  calls per chunk (length prefix + ciphertext) — 2× 1024 = 2048
  write(2) calls per 1 GB encrypt, 1024 per 1 GB decrypt. Each
  write(2) is a syscall + a kernel pagecache copy. At 1 MiB-per-write
  the syscall amplification is small (~3 µs each = ~3 ms total) but
  paired with the per-chunk Vec alloc (R10-P3) the cumulative effect
  is two unnecessary copies per chunk on the host (Vec → user-space
  buffer → kernel buffer).
- **Hot path**: every AEAD encrypt + decrypt.
- **Estimated p50 delta**: ~3–10 ms per pass on 1 GB. Trivial alone;
  fold into the same edit as R10-P3.
- **Action**: Wrap `dst` in `BufWriter::with_capacity(1 << 20, dst)`
  for both paths. (The reads on input are 1 MiB-aligned already so
  `BufReader` adds nothing.)

### [R10-P6] `wait_for_alloc_running_blocking` + `wait_for_livez_blocking` polling at 250 ms / 150 ms on a fresh ureq connection per poll (IMPORTANT, performance-r10; refines r9 #3)

- **Files**:
  - `crates/sandbox/src/restore_handler.rs:1335-1360` (nomad_post/get/delete_blocking — fresh `ureq::Request` per call)
  - `crates/sandbox/src/restore_handler.rs:1398-1485` (poll loops)
- **Symptom**: r9 #3 noted that no `ureq::Agent` is cached. Confirmed
  open at e7b3278b. Refining the cost: at typical wake of ~3 s alloc-
  running poll + ~3 s livez poll = (3000/250) + (3000/150) = 12 + 20
  = **32 fresh HTTP connection setups per wake**. Plus 1 for the
  initial POST and 1 for `clock_resync` = 34.
  - Nomad agent talks plain HTTP locally (no TLS handshake): ~1–3 ms
    per connection (TCP 3WHS + socket setup).
  - Agent /livez goes to the guest network: still plain HTTP at the
    in-VM agent, ~1–3 ms RTT each.
  Total connection-setup overhead per wake: **~30–100 ms** burned on
  TCP handshakes that a pooled `ureq::Agent` would amortize to ~1
  connection.
- **Hot path**: every wake. At c=20 stress 32 × 20 = 640 extra TCP
  handshakes/wake-cycle hitting the local Nomad agent — could trigger
  ephemeral-port pressure if many wakes overlap, but at 250 ms cadence
  the steady-state rate is bounded.
- **Estimated p50 delta**: ~30–100 ms/wake saved. Sub-100 ms but
  trivial code change (`static AGENT: OnceLock<ureq::Agent>` +
  `AGENT.get_or_init(|| ureq::AgentBuilder::new().build())`). Stacks
  on the spawn_blocking-of-the-poll wins.
- **Action**: Cache a single `ureq::Agent` in a `OnceLock` at module
  scope; route all `nomad_*_blocking` + `clock_resync` calls through
  it. Honor existing per-call timeouts via `Request::timeout`.

### [R10-P7] `clock_resync_random_hex` allocates one `String` per byte via `format!("{b:02x}")` collected into a String (MINOR, performance-r10; refines r9 #7)

- **Files**: `crates/sandbox/src/restore_handler.rs:1635-1643`
- **Symptom**: r9 #7 noted the format-loop overhead. At
  `n_bytes = 32` (the challenge field) the function executes 32 ×
  `format!("{b:02x}")` calls, each allocating a 2-byte `String` that
  is then collected into the result. Per wake = one challenge + one
  nonce = 48 short-lived `String` allocations. Aggregate sub-100 µs,
  but every wake pays the allocator round-trip.
- **Hot path**: every wake (called from `clock_resync_post_restore`).
- **Estimated p50 delta**: <100 µs/wake. Negligible.
- **Action**: Replace with `hex::encode(&buf)` — already in the
  workspace `Cargo.toml` (used by `snapshot_aead.rs` and
  `snapshot_store_gcs.rs`). One-line change.

## Carry-forward (open from earlier rounds)

- **[R9-P1] AEAD-active wake-path `get` discards R5-P1 hard-link
  zero-copy**: STILL OPEN at `snapshot_aead.rs:619-664`. The inner
  hard-link is into `stage/`, then `decrypt_to` writes a full
  plaintext copy into `target_dir/memory-ranges`. The biggest
  AEAD-active wake-path lever (~0.5–1.5 s/wake).
- **[R9-P2] `verify_metadata_only` zero callers**: STILL OPEN.
  `Grep '\.verify(_metadata_only)?\('` over `crates/sandbox/src/
  sweep.rs` returns zero matches — confirmed by direct Grep at
  audit-time. The fast-path is dead code in production.
- **[R9-P3] AEAD 3-pass fusable on snapshot put**: STILL OPEN. See
  R10-P1 / R10-P4 — this finding folds into the broader fused-
  pipeline action above.
- **[R9-#3] No persistent `ureq::Agent`**: STILL OPEN — see R10-P6
  for the refined cost.
- **[R9-#6] 64 KiB scratch buffer allocated per call inside the
  ARTIFACT_FILES loop**: STILL OPEN at `snapshot_store.rs:207`.
  Identical line at `snapshot_store_gcs.rs:981`. Trivial hoist.
- **[R9-#7] `clock_resync_random_hex` `format!("{:02x}")` loop**:
  STILL OPEN — see R10-P7.
- **[R9-#8] `chunk_aad` allocates a 13-byte `Vec` per chunk**:
  STILL OPEN at `snapshot_aead.rs:311-316`. 2048 short-lived 13-byte
  Vec allocations per round-trip.

## Two most critical NEW citations

- `snapshot_store_gcs.rs:1066-1093` — `compio::runtime::spawn(async
  move { l2.put(...) })`. The TODO comment in this block specifically
  flags it ("When real GCS lands, wrap in spawn_blocking like the
  rest of the controller"). Real GCS HAS landed; the wrap-with-
  spawn_blocking edit was never made. Closes R7-P2 as
  confirmed-still-open.
- `snapshot_store_gcs.rs:539-622` + `snapshot_aead.rs:363-449` +
  `snapshot_store.rs:184-223` — the AEAD-active + GCS-enabled
  snapshot path reads `memory-ranges` 5 times sequentially. Three of
  those reads are fusable into the streaming encrypt/upload pipes;
  one belongs to the L1 SHA which is already known once L2 spawns;
  the unavoidable two are the encrypt input + the upload output.

## Ranked next-biggest perf lever

1. **Wrap L2 GCS put in spawn_blocking** (R10-P2): two-line diff;
   removes a parked-executor-thread liability under load. Unknown
   p50 delta but a clear correctness-of-concurrency win.
2. **Fuse encrypt + canonical-SHA + L2-side SHA into the streaming
   pipes** (R10-P1 + R10-P4 + r9 #2): ~1.5–2.5 s/snapshot saved at
   c=N stress; drops local disk I/O per snapshot from ~6 GB to ~3 GB
   per 1 GB guest. Largest snapshot-path lever.
3. **Eliminate the second 1 GB write on AEAD-active wake** (R9-P1):
   ~0.5–1.5 s/wake saved; restores R5-P1's hard-link advantage to
   the AEAD-active deploy posture. Largest wake-path lever.
4. **`encrypt_in_place_detached` + `decrypt_in_place_detached`**
   (R10-P3): removes 2048 × ~1 MiB heap allocations per AEAD round-
   trip. Plausible 100–400 ms saved per round-trip; unknown without
   measurement.
5. **Cached `ureq::Agent`** (R10-P6 / r9 #3): ~30–100 ms/wake saved.
6. **Wire `verify_metadata_only` into sweep** (R9-P2): only relevant
   if integrity sweep is in scope this cycle. Dead code otherwise.

## Wake-path latency budget (current estimate based on code, NOT measurement)

| Component | Estimate | Source |
|---|---|---|
| `submit_restore_job` (POST + wait_for_alloc_running blocking, in spawn_blocking) | ~2.0–3.5 s | `restore_handler.rs:484-500` + `:1077-1112` |
| `wait_for_livez` (spawn_blocking) | ~1.0–3.0 s | `restore_handler.rs:509-518` + `:1466-1485` |
| `store.get` AEAD-active path (hard-link to stage + 1 GB decrypt-write to target) | ~1.0–2.0 s | `snapshot_aead.rs:619-664` (R9-P1) |
| `clock_resync` (spawn_blocking; one /dev/urandom read + one signed POST) | ~0.05–0.15 s | `restore_handler.rs:1530-1616` |
| `register_restored` + state.write insert | <0.01 s | `nomad_ch.rs:1659-1690` |
| pg awaits (read_snapshot_row + status CASes) | ~0.05–0.2 s | `restore_handler.rs:212-247, 600-612` |
| Connection-setup overhead (~32 fresh ureq calls × 1–3 ms TCP 3WHS) | ~0.03–0.10 s | R10-P6 |

**Estimated wake p50 (AEAD active, L1 hit)**: **~4.1–9.0 s**.

Mostly unchanged from r9; the new R10-P2 won't move p50 directly but
will shrink p95–p99 tail under c=20 stress. The R9-P1 fix is the only
single edit that could meaningfully move the AEAD-active p50 baseline
into the 3.5–7.0 s range.
