# Sandbox/snapshot-restore — performance r11 review

Date: 2026-05-25 (UTC)
HEAD at audit: `d2cfcb34` (worktree currently at `c8000537`, one
docs-only commit ahead — `c8000537` is a comment-only change to
`crates/sandbox-agent/src/handlers/version.rs:49`; no impact on the
findings below).
Round 11 of N.

Static review only; no fresh cluster numbers. Baseline (r8 / Appendix F
c=4): snapshot p50 50573 ms, wake p50 9235 ms.

## Summary

**4 findings** (1 CRITICAL, 2 IMPORTANT, 1 MINOR), all new since r10.
The single biggest new lever is **R11-P1**: every `Database` method
opens a fresh `Pool` + does a full PG TCP/STARTUP/auth handshake per
call. The wake path pays this **5× per restore**; the
transient-takeover sweep pays it **1 + N times per tick** (where N is
the orphaned-row count). This is a TODO documented at `db.rs:494-507`
that no prior performance round flagged — it is by far the largest
remaining low-effort wake-path lever after R9-P1.

The other new findings (R11-P2 wake-path GCS download write loop; R11-P3
`canonical_artifact_sha256` + `sha256_file` missing `BufReader`; R11-P4
sweep `SandboxRow::clone()` allocation profile) are smaller in
magnitude but stack with R10-P1 / R10-P5 in the same edit area.

R10-P2 (Tiered::put → spawn_blocking) is confirmed CLOSED. All other
R10 carry-forward (R10-P1, R10-P3..P7) and R9-P1 / R9-P3 remain OPEN.

## Findings (NEW since r10)

### [R11-P1] Every `Database` method opens a fresh PG `Pool` + connection per call → 5 fresh PG handshakes per wake, 1+N per transient-takeover tick (CRITICAL, performance-r11)

- **Files**:
  - `crates/sandbox/src/db.rs:492-514` (`open_pool`, with documented
    TODO: "every method on Database calls this and drops the pool
    inside the same future. That round-trips a TCP connect + auth
    handshake on every call")
  - `crates/sandbox/src/db.rs:516-535` (`pool_app` / `pool_audit` —
    aliases that re-open as well)
  - Call sites in the wake path:
    - `restore_handler.rs:213` `db.get_sandbox_row(...).await` (1)
    - `restore_handler.rs:336-339` `db.pool_app().await` inside
      `read_snapshot_row` (1)
    - `restore_handler.rs:246-247` `db.update_sandbox_status` (CAS to
      `restoring`) (1)
    - `restore_handler.rs:624-626` `db.update_sandbox_status` (CAS to
      `running`) (1)
    - `restore_handler.rs:627` `db.clear_snapshot_metadata` (1)
  - Call sites in the transient-takeover sweep:
    - `db.rs:2407-2411` `transient_state_lease_expired_sandboxes`
      opens once per sweep tick
    - `db.rs:2500-2532` `claim_orphan_transient_for_recovery` opens
      once PER ROW found by the sweep
- **Symptom**: `compio_postgres::Pool::connect_with_config`
  (`crates/compio-postgres/src/pool.rs:264-300`) does NOT return
  cheaply — it eagerly opens `config.min_idle.max(1)` connection(s)
  upfront, with TCP connect + PG STARTUP + auth, then drops the whole
  pool when `open_pool`'s future returns. The Pool's housekeeper /
  warm-up retry logic exist precisely to amortize this over many
  `pool.get()` calls — but `Database` discards the Pool after one
  `client.query_*` and rebuilds it on the next method.
- **Hot path**:
  - **Wake** (5 fresh PG handshakes per restore, all on the critical
    path). Two of the five (`update_sandbox_status` after
    `wait_for_livez` + `clear_snapshot_metadata`) are sequential and
    on the tail.
  - **Snapshot** (4 fresh PG handshakes per snapshot:
    `get_sandbox_row`, `update_sandbox_status`, `update_snapshot_meta`,
    rollback-only `update_sandbox_status`).
  - **Transient-takeover sweep** (30 s cadence): 1 +
    `claim_orphan_transient_for_recovery_calls`. If 100 abandoned rows
    pile up, that's **101 fresh PG handshakes every 30 s**.
  - **Idle-eviction sweep** (300 s cadence): 1 +
    per-attempted-row CAS handshakes through `update_sandbox_status` in
    the inner `snapshot_sandbox` pipeline.
- **Estimated p50 delta**: unknown without measurement. Code-derived
  basis: a PostgreSQL TCP+STARTUP+SCRAM-SHA-256 handshake on LAN to a
  managed PG is typically 2–15 ms depending on TLS and password method.
  With 5 sequential PG handshakes per wake the avoidable cost is
  **~10–75 ms/wake** at the median, larger at the tail. Compared with
  the ~9 s baseline this is sub-1% on p50 — but every other live
  control-plane method (`stop`, `delete`, heartbeat, etc.) carries the
  same multiplier, and the sweep loops carry an N× multiplier where
  N is the candidate-row count. **The lever is larger as fleet size
  grows; the in-tree TODO comment explicitly notes this is "correct,
  just slow on the hot path".**
- **Action**: per the TODO at `db.rs:494-507`, hoist the pool to a
  per-compio-thread `thread_local!` `Rc<Pool>`, lazily initialised on
  first use, with `Pool::start_housekeeper` called once at init. The
  `compio_postgres::Pool` is `!Send + !Sync` (per its `RefCell`/`Cell`
  interior), which the TODO already notes; thread-locals satisfy
  ntex's `Send + Clone` factory bound as long as no `Pool` value
  crosses worker boundaries. Two implementation options:
  1. `thread_local! { static POOL: RefCell<Option<Rc<Pool>>> = ... }`
     in `Database`, returning a cloned `Rc<Pool>` per call; build the
     `Pool` lazily.
  2. Build the Pool eagerly at `AppState::from_config` per ntex worker
     thread and stash it in worker-local state.
  Either way, hoisting eliminates 4 of the 5 PG TCP handshakes per
  wake (one initial cost amortized across all subsequent calls within
  the thread).

### [R11-P2] Wake-path GCS download writes a 1 GB stream with `std::io::copy` (8 KiB buffer) — 131072 write(2) syscalls per 1 GB memory-ranges download (IMPORTANT, performance-r11)

- **Files**: `crates/sandbox/src/snapshot_store_gcs.rs:438-445`
- **Symptom**: On an L1 miss the wake path falls through to
  `download_to_disk`. The implementation is:
  ```rust
  let mut f = std::fs::File::create(dest)?;
  let mut reader = r.into_reader();
  std::io::copy(&mut reader, &mut f)?;
  ```
  `std::io::copy` on a generic `Read` + `Write` pair uses an 8 KiB
  stack buffer (stdlib default — its `BufferedCopySpec` short-circuits
  only when one side is `BufRead`). A 1 GB `memory-ranges` download
  therefore triggers ~131072 `read(2)`s from the ureq stream + ~131072
  `write(2)`s into `dest` — each crossing the user/kernel boundary +
  a kernel pagecache copy. Counterpart to R10-P5 on the snapshot path,
  but on the wake-path hot loop and at 16× higher syscall granularity
  (R10-P5 was 1 MiB-per-write; this is 8 KiB-per-write).
- **Hot path**: every wake that misses L1 (cross-worker takeover, or
  first wake after worker restart). Combined with the AEAD-active path
  that then re-reads + decrypts the just-downloaded file, the L1-miss
  wake path is the worst case for sequential SSD pressure under c=N
  stress.
- **Estimated p50 delta**: unknown — syscall amplification at 8 KiB
  is real but the file I/O is overlapped with the network stream; the
  bottleneck is usually the network or the SSD throughput, not the
  syscall rate. Code-derived ceiling: at ~3 µs per write(2) on Linux,
  131k writes = ~400 ms of pure syscall overhead. Whether that
  serializes against the network or pipelines depends on ureq's
  buffering — but a `BufWriter::with_capacity(1<<20, dst)` shrinks
  the write-side syscall count to ~1024 and is a two-line edit.
- **Action**: Wrap `dest` in `BufWriter::with_capacity(1 << 20, f)`
  before calling `std::io::copy`. Also wrap `reader` in a
  `BufReader::with_capacity(1 << 20, reader)` so the read side fuses
  to 1 MiB chunks. Mirror the fix at the GCS resumable-upload write
  path if it has the same shape (didn't audit; flagged for the next
  round).

### [R11-P3] `canonical_artifact_sha256` + `sha256_file` (snapshot_store_gcs.rs) use unbuffered 64 KiB reads — collide with R10-P1's 5-pass disk walk (IMPORTANT, performance-r11)

- **Files**:
  - `crates/sandbox/src/snapshot_store_gcs.rs:966-997`
    (`canonical_artifact_sha256` — duplicate of the L1 helper but
    WITHOUT the 1 MiB `BufReader` that R5-P1 added to `snapshot_store.
    rs:184-223`)
  - `crates/sandbox/src/snapshot_store_gcs.rs:506-521` (`sha256_file`
    — same shape, no `BufReader`)
- **Symptom**: r9 #6 carry-forward called out the 64 KiB scratch
  buffer hoist; r11 adds the missing-BufReader observation. The L1
  helper (`snapshot_store.rs:205-214`) wraps the file in
  `BufReader::with_capacity(1 << 20, f)`. The GCS-side copies don't —
  they `std::fs::File::open(&path)` directly and `f.read(&mut buf)` in
  a loop on a 64 KiB heap-allocated scratch buffer. On 1 GB memory-
  ranges this is 16384 unbuffered read(2)s vs. 1024 with a 1 MiB
  BufReader — a 16× syscall amplification. The L1 fix exists upstream
  in the same crate (R5-P1, commit referenced in `snapshot_store.rs:
  199-204`); the GCS copy was left behind.
- **Hot path**: every snapshot put (R10-P1 line 4 — `sha256_file` per
  artifact file) + every snapshot put (R10-P1 line 3 —
  `canonical_artifact_sha256`). Both already flagged in R10-P1 as
  fusable, but until that fusion lands the unbuffered reads compound
  R10-P1's I/O bill.
- **Estimated p50 delta**: ~3–10 ms per pass on 1 GB at typical
  syscall rates. Small alone; matters because R10-P1's 5 reads bake
  this in 3× per snapshot. If R10-P1 is fixed by fusing reads, this
  finding vanishes; if not, the BufReader hoist is a 2-line edit per
  helper.
- **Action**:
  1. In `canonical_artifact_sha256` at line 980, replace
     `let mut f = std::fs::File::open(&path)?;` with
     `let f = std::fs::File::open(&path)?; let mut f = BufReader::
     with_capacity(1 << 20, f);` Either delete the per-call scratch
     `vec![0u8; 64 * 1024]` (use a stack `[u8; 64 * 1024]` since the
     read loop bounds it) or hoist it outside the `ARTIFACT_FILES`
     loop (r9 #6).
  2. Same change in `sha256_file` at line 507.

### [R11-P4] Sweep `snapshot_rows_chunked` clones each `SandboxRow` (7 owned `String`s) before await, even on rows whose `snapshot_one` succeeds (MINOR, performance-r11)

- **Files**: `crates/sandbox/src/sweep.rs:494-526` (`snapshot_rows_
  chunked`)
- **Symptom**: The R3-Q1 fix that incrementally appends to `attempted`
  before await (lines 524-526):
  ```rust
  for (row, _) in &parsed {
      attempted.push((*row).clone());
  }
  ```
  clones EVERY `SandboxRow` in the chunk regardless of whether the
  caller actually consumes `attempted` for anything beyond a debug log
  count. `SandboxRow` carries 7 owned `String`s (`sandbox_id`,
  `user_id`, `project_id`, `backend`, `agent_url`, `host_id`, `key_fp`)
  — that's ~7 heap allocations per row, per chunk. The sole caller
  (`run_idle_eviction_once` at `sweep.rs:464`) passes the returned Vec
  to:
  - the sweep-loop debug log at `sweep.rs:603-606` (uses only
    `attempted.len()`)
  - the pg-gated test at `sandbox_pg_e2e.rs` (asserts on len + ids)
  Neither needs the full `SandboxRow` — `len()` would suffice for the
  log, and only the typed-id string is needed for the test assertion.
- **Hot path**: every idle-eviction sweep tick (default 300 s
  cadence), per row in the batch. At default `IDLE_BATCH_LIMIT=100`
  this is ~700 small heap allocations every 5 minutes — negligible
  absolute, but the clone is also a partial-shutdown-correctness
  liability if `SandboxRow` grows new fields (each new field pays the
  clone cost forever).
- **Estimated p50 delta**: unmeasurable (sub-ms per sweep tick at
  300 s cadence). Listed for the inventory only; deferrable behind
  P1–P3.
- **Action**: Change `attempted: Vec<SandboxRow>` to
  `attempted: Vec<String>` (carry only the typed-id) or
  `Vec<Uuid>`. Update the caller's log + the pg-gated test to match.
  Tightens the invariant that "attempted" is just a list of "rows we
  touched", not a full row snapshot.

## Closed by recent commits

- **[R10-P2 / R7-P2]**: `compio::runtime::spawn_blocking` on
  `TieredSnapshotStore::put` L2 detach — closed at `6f314025`.
  Confirmed at `crates/sandbox/src/snapshot_store_gcs.rs:1066`: the
  spawn now wraps the inner `l2.put(...)` call in `spawn_blocking`
  (the outer `compio::runtime::spawn` is gone). The TODO comment
  about wrapping was acted on. ✓

## Carry-forward (still open from r10 / r9)

| Finding | Status | File:line |
|---|---|---|
| **R10-P1** AEAD-active snapshot reads memory-ranges 5×       | OPEN | `snapshot_aead.rs:377-465`, `snapshot_store.rs:184-223`, `snapshot_store_gcs.rs:540-622`, 966-997 |
| **R10-P3** `cipher.encrypt`/`decrypt` allocates a fresh Vec per chunk | OPEN | `snapshot_aead.rs:411-446`, 529-572 |
| **R10-P4** `GcsSnapshotStore::put` recomputes canonical SHA  | OPEN | `snapshot_store_gcs.rs:559-560`, 1066 |
| **R10-P5** AEAD encrypt + decrypt write to raw `File` (no `BufWriter`) | OPEN | `snapshot_aead.rs:403, 529` |
| **R10-P6** ~32 fresh `ureq` connections per wake (no pooled `Agent`) | OPEN | `restore_handler.rs:1387-1412, 1450-1537` |
| **R10-P7** `clock_resync_random_hex` builds via `format!("{b:02x}")` loop | OPEN | `restore_handler.rs:1687-1695` (line 1694) |
| **R9-P1** AEAD-active wake-path get discards R5-P1 hard-link | OPEN | `snapshot_aead.rs:633-678` (lines 660-664: full plaintext copy to target) |
| **R9-P3** AEAD 3-pass fusable on snapshot put | OPEN (subsumed by R10-P1) | — |
| **R9-#6** 64 KiB scratch buffer inside `ARTIFACT_FILES` loop | OPEN (refined by R11-P3) | `snapshot_store.rs:207`, `snapshot_store_gcs.rs:509, 981` |
| **R9-#8** `chunk_aad` allocates a 13-byte Vec per chunk | OPEN | `snapshot_aead.rs:325-330` |

## Notes on focus-area questions

**(1) `Persistence::unseal` per-restore work**: examined
`persist.rs:692-702`. The `spawn_blocking` → `unseal_one` path reads a
~250-byte file, AEAD-decrypts (~150 bytes), and `serde_json::from_slice`
parses. No unnecessary work; not a finding.

**(2) Sweep loops**: see R11-P4 (sweep allocs) and R11-P1 (per-row PG
handshake in the transient-takeover sweep). The takeover loop is
fundamentally serial-per-row in pg-roundtrip terms.

**(3) `claim_orphan_transient_for_recovery` roundtrips**: 1 UPDATE per
row, plus 1 SELECT in the CAS-miss branch. The N+1 concern is real but
not because of redundant lookups — it's because **each call opens a
fresh PG pool** (R11-P1 covers this). Same row hit twice (UPDATE +
SELECT) reuses the same checked-out client within the function, so the
intra-function shape is fine; the cross-function shape is what bleeds.

**(4) `do_restore_inner` await points**: counted 7 awaits inside (lines
429, 521, 539, 582, 593, 626, 627) — `store.get`, `submit_restore_job`,
`wait_for_livez`, `persist.unseal`, `clock_resync_post_restore`,
`update_sandbox_status` (→running), `clear_snapshot_metadata`. Plus 3
outside in `restore_sandbox` (`get_sandbox_row`, `read_snapshot_row`,
`update_sandbox_status` →restoring). All 5 db.* awaits are R11-P1
candidates. The R8-A3-5 and R10-C1+C2 commits added `spawn_blocking`
wrappers around already-existing sync work and a rollback hop — neither
introduces a new wake-path await that wasn't previously sync-blocking
on the same thread; both are pure scheduling improvements.

**(5) AEAD-active wake under c=20**: code-derived only, NOT measured.
At c=20 the worker drives up to 20 concurrent `compio::runtime::spawn_
blocking(store.get)` calls. Each is a ~2 GB SSD I/O bill (1 GB
ciphertext read + 1 GB plaintext write per R9-P1). With 4
simultaneously-active AEAD decrypts on the same SSD, the kernel
pagecache provides **zero help** because each restore reads a distinct
`memory-ranges` artifact (each sandbox has its own). The Linux block
layer's CFQ/mq-deadline scheduler interleaves 4 sequential read
streams into a single device queue; observed effective bandwidth per
stream is typically 30–40% of solo bandwidth on cheap NVMe and ~50%
on enterprise NVMe with parallel read paths. The disk queue
serializes; the kernel cache does not help. This is the strongest
case for R9-P1 (eliminate the second 1 GB write on the AEAD wake
path) — it would cut the per-wake I/O in half exactly when contention
matters most.

**(6) `String` / `Vec` allocations in hot loops**:
- snapshot_aead encrypt loop (R10-P3 / R10-P5 / R9-#8) — already
  flagged.
- restore_handler `wait_for_alloc_running_blocking` poll loop
  (lines 1466-1483) does a couple of `.to_string()` per JSON alloc
  parse. ~12 polls × 2 allocs = 24 short-lived allocs per wake;
  sub-µs; not worth flagging.
- sweep `SandboxRow::clone` (R11-P4 above).
- The R11-P1 pool-creation path implicitly allocates a `Vec` of
  `PoolEntry`, a `VecDeque<Waiter>`, `PoolMetrics`, `String` for the
  URL — all dropped after one query.

**(7) R10-P2's fix correctness**: confirmed at `snapshot_store_gcs.rs:
1066` — `compio::runtime::spawn_blocking(move || { match l2.put(...)
{...} })` is now in place; the prior `compio::runtime::spawn(async
move {...})` shape is gone. The outer `.detach()` preserves the fire-
and-forget semantics. Two-line diff landed as intended.

## Ranked next-biggest perf lever (updated)

1. **Hoist `Database` Pool to a per-thread `Rc<Pool>`** (R11-P1):
   code-derived 4× PG handshake savings per wake, larger savings on
   every other db method + sweep loops. By far the largest low-effort
   lever after R9-P1.
2. **Eliminate the second 1 GB write on AEAD-active wake** (R9-P1):
   ~0.5–1.5 s/wake saved; pairs with the c=20 SSD-contention concern
   in focus area (5).
3. **Fuse encrypt + canonical-SHA + L2-side SHA into the streaming
   pipes** (R10-P1 + R10-P4 + r9 #2): ~1.5–2.5 s/snapshot saved at
   c=N stress.
4. **`encrypt_in_place_detached` + `decrypt_in_place_detached`**
   (R10-P3): plausible 100–400 ms saved per AEAD round-trip;
   measurement-dependent.
5. **`BufWriter` on the GCS wake-path download** (R11-P2): ~400 ms of
   syscall-overhead ceiling; pipelines with the network so the actual
   win may be smaller.
6. **Cached `ureq::Agent`** (R10-P6 / r9 #3): ~30–100 ms/wake saved.
7. **`BufReader` on GCS-side SHA helpers** (R11-P3): subsumed by
   R10-P1 if that fusion lands; otherwise ~3–10 ms/pass.
8. **Sweep allocation cleanup** (R11-P4): sub-ms; correctness/code-
   quality lever more than perf.

## Updated wake-path latency budget (code-derived; NOT measured)

| Component | Estimate | Source |
|---|---|---|
| `submit_restore_job` (spawn_blocking) | ~2.0–3.5 s | `restore_handler.rs:508-524` + `:1450-1508` |
| `wait_for_livez` (spawn_blocking) | ~1.0–3.0 s | `restore_handler.rs:533-542` + `:1518-1537` |
| `store.get` AEAD-active path (hard-link to stage + 1 GB decrypt-write to target) | ~1.0–2.0 s | `snapshot_aead.rs:633-678` (R9-P1) |
| `clock_resync` (spawn_blocking; one /dev/urandom read + one signed POST) | ~0.05–0.15 s | `restore_handler.rs:1582-1668` |
| `register_restored` + state.write insert | <0.01 s | `nomad_ch.rs:1659-1690` |
| pg awaits (5 fresh handshakes: get_sandbox_row, read_snapshot_row, update_sandbox_status×2, clear_snapshot_metadata) | ~0.05–0.2 s | R11-P1 |
| Connection-setup overhead (~32 fresh ureq calls × 1–3 ms TCP 3WHS) | ~0.03–0.10 s | R10-P6 |

**Estimated wake p50 (AEAD active, L1 hit)**: **~4.1–9.0 s**. Same
range as r10 — none of the new findings shift the median by an
estimable amount. R11-P1 saves ~10–75 ms in the median; the rest are
tail / I/O-amplification levers.

The R9-P1 fix remains the only single edit that could meaningfully
move the AEAD-active p50 baseline into the 3.5–7.0 s range. R11-P1 is
the largest **non-wake** perf lever (sweep loops, snapshot path, every
control-plane RPC); flagging here because the perf budget for those
flows is just as relevant as the wake-path budget once the wake-path
peak is exercised.
