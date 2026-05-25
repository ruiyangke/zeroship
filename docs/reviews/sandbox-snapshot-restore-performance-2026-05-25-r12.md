# Sandbox/snapshot-restore — performance r12 review

Date: 2026-05-25 (UTC)
HEAD at audit: `b6a99fb9` (one docs-only / one cluster-result commit
ahead of `9f1dfc99` named in the brief; neither touches the perf
surface).
Round 12 of N.

Static review only; T-8b-smoke-retry FAILED on CREATE (Bug C-1 —
nomad-driver-ch v2 invokes `cloud-hypervisor --config`, a flag CH v51.1
does not accept), so no fresh single-cycle wake/snapshot timings landed
this cycle. Baseline (r8 / Appendix F c=4) remains: snapshot p50
50573 ms, wake p50 9235 ms.

## Summary

**1 finding** (1 IMPORTANT, 0 critical, 0 minor), new since r11. The
single new lever is **R12-P1**: R11-P2's BufWriter wrap on
`download_to_disk` left the READ side of the GCS `std::io::copy`
unbuffered. `ureq::BodyReader` is `Read`-only (not `BufRead`), so
stdlib's `BufferedCopySpec` keeps the 8 KiB scratch buffer for source
reads; only the write-side syscall count was collapsed. The
counterpart 1-line `BufReader::with_capacity(1 << 20, reader)` would
fuse stdlib's `<R: BufRead>` specialization that drains directly from
the source's internal buffer into the writer.

No fresh CRITICAL/MINOR findings. R11-P1 carries forward with its
architecture-confirmed thread-local recipe; all other r10/r9
carry-forward unchanged.

## Findings (NEW since r11)

### [R12-P1] GCS `download_to_disk` `std::io::copy` still pays 8 KiB read-side syscall amplification — R11-P2 only fixed write side (IMPORTANT, performance-r12)

- **Files**:
  - `crates/sandbox/src/snapshot_store_gcs.rs:438-458` (post-R11-P2
    download body)
- **Symptom**: R11-P2 (commit `3d5c527f`) added
  `BufWriter::with_capacity(1 << 20, f)` on the destination side of
  `std::io::copy(&mut reader, &mut writer)`. That collapses the
  write-side syscall count from ~131072 (8 KiB) to ~1024 (1 MiB) for a
  1 GB download. **But the source `reader = r.into_reader()` is
  `ureq`'s `BodyReader` — a `Read`-only wrapper, not `BufRead`.**
  Stdlib's `io::copy::stack_buffer_copy` falls through to the generic
  8 KiB scratch-buffer loop whenever the source is not `BufRead`; the
  `BufWriter` on the dest only affects the destination write
  granularity, not the source read granularity. Net result:
  - **read(2)s from `ureq` socket reader: still ~131072 × 8 KiB**
    (unchanged from pre-R11-P2 — the read-side win was not captured)
  - write(2)s into the file: ~1024 × 1 MiB (R11-P2 fix in effect)

  Wrapping `reader` in `BufReader::with_capacity(1 << 20, reader)`
  flips `io::copy` into the `<R: BufRead, W: Write>` specialization
  (`BufferedReaderSpec` / `copy_to_buffered`), which drains
  `r.fill_buf()` directly into `w.write_all(...)` — no intermediate
  scratch buffer copy, and the BufReader's `fill_buf` issues 1 MiB
  socket reads against `ureq`'s underlying reader rather than 8 KiB.
- **Hot path**: every wake that misses L1 — same scope as the R11-P2
  write-side fix. Cross-worker takeover + first wake after a worker
  restart are the two driver scenarios.
- **Estimated p50 delta**: unknown — syscall amplification on the
  read side, but pipelined with the network. Code-derived ceiling
  mirrors R11-P2's: ~131k reads × ~1-3 µs per kernel→user copy ≈
  ~200-400 ms of theoretical syscall overhead, but the actual ceiling
  is bounded by network bandwidth and ureq's internal buffering of
  the TLS stream. The fix is symmetric with R11-P2 and a 1-line
  diff; sized to land in the same commit had R11-P2 caught both
  sides.
- **Action**: Wrap the reader symmetrically:
  ```rust
  let mut reader = std::io::BufReader::with_capacity(1 << 20, r.into_reader());
  ```
  alongside the existing `BufWriter::with_capacity(1 << 20, f)`.
  Stdlib's specialization then fires (no scratch buffer; copies
  reader's internal slice directly to writer's). This change is the
  exact mirror of R11-P3's `BufReader` wrap on the SHA helpers — same
  type, same capacity, applied to the GCS-download path.

  Optional: also reconsider `std::io::copy` for the file-create
  side. With BufReader on src + BufWriter on dst, stdlib's
  `BufferedReaderSpec` already uses `write_all` on the dest. If the
  destination flush cost matters, swap to a manual `loop { let buf =
  reader.fill_buf()?; if buf.is_empty() { break; } let n =
  writer.write(buf)?; reader.consume(n); }` — but the 1-line wrap is
  sufficient for the syscall-collapse win.

## R11-P1 thread-local feasibility (confirmed via Pool source)

Reading `crates/compio-postgres/src/pool.rs:14-17`:

> Single-threaded: uses `Cell`/`RefCell` (no atomics, no Mutex). The
> pool is `!Send` because compio's TcpStream uses `Rc` internally —
> the type system enforces that all access is on one thread.

And `start_housekeeper` at line 341:

```rust
pub fn start_housekeeper(self: &std::rc::Rc<Self>) {
    let weak = std::rc::Rc::downgrade(self);
    compio::runtime::spawn(async move { ... });
}
```

Confirms two design constraints:

1. **`Pool: !Send + !Sync`** is enforced by the type system, not by
   accident. `Pool` holds `RefCell<Vec<PoolEntry>>` + `Cell<usize>` +
   `RefCell<VecDeque<Waiter>>`. `PoolEntry` holds a `Client` that in
   turn owns `compio::net::TcpStream` (`!Send` via internal `Rc`).
   No way around this with `Arc`/`Mutex` — the underlying compio
   socket is single-thread-owned by construction.

2. **`start_housekeeper` takes `&Rc<Self>`**, not `&Self` — strong
   signal from the Pool's own API that the canonical shape is
   `Rc<Pool>` (the housekeeper holds a `Weak<Pool>` so the pool
   self-drops when the last `Rc` goes away).

**Conclusion**: the architecture-r11 sketch is correct and the only
viable shape is per-compio-worker `thread_local!<RefCell<Option<Rc<Pool>>>>`:

```rust
thread_local! {
    static POOL_APP: RefCell<Option<Rc<Pool>>> = RefCell::new(None);
    static POOL_AUDIT: RefCell<Option<Rc<Pool>>> = RefCell::new(None);
    static POOL_GDPR: RefCell<Option<Rc<Pool>>> = RefCell::new(None);
}

impl Database {
    async fn open_pool(&self) -> Result<Rc<Pool>> {
        // 1. Try POOL_APP.with(|c| c.borrow().clone()) — return Rc clone if Some.
        // 2. Else: build Pool::connect_with_config(...).await, wrap in Rc,
        //    call start_housekeeper(&rc), stash in POOL_APP, return clone.
    }
}
```

Notes for the implementer (R11-P1 sprint):

- `Database` itself stays `Send + Sync + Clone` (only `DbConfig`
  fields). The thread-local lives at module scope, not on the
  struct, so the `Send + Clone` ntex-factory bound is unaffected.
- `Rc<Pool>` is the right inner type. `compio-postgres` ships no
  shareable handle; `Rc` is the cheap clone that lets multiple call
  sites on a single worker share one pool without lifetime
  plumbing.
- `start_housekeeper` MUST be called exactly once per `Pool`
  instance (the housekeeper task is detached and uses `Weak<Pool>`
  to self-terminate). Calling it inside the first-init branch of
  `open_pool` satisfies this.
- The `pool_audit` / `pool_gdpr` aliases are distinct DSNs (audit
  role / GDPR role), so each needs its own thread-local cell. Three
  separate `thread_local!`s, not one.
- `dsn_app`/`dsn_audit`/`dsn_gdpr` would need to be captured into
  each cell at first use (or canonicalized into a tuple `(dsn,
  Rc<Pool>)` so a stale config reload would invalidate).
- The `compio_postgres::Pool` already handles connection
  rotation/eviction internally; the per-worker pool benefits from
  the existing housekeeper logic for free.

**Estimated savings unchanged from r11**: ~10-75 ms median per wake
(4 of 5 PG handshakes amortized), proportionally larger on the sweep
loops (N+1 per transient-takeover tick collapses to 1).

## Wake-path latency budget (updated for r12)

No measured cluster timings landed since r11. Updates vs r11's table:

- **R11-P2 + R11-P3 closed at `3d5c527f`**: the GCS download wake-path
  write-side and the SHA helpers each gained the 1 MiB BufReader/
  BufWriter wrap. Code-derived: the L1-miss wake's syscall-overhead
  ceiling drops by ~80-90% on the write side (was ~131k writes,
  now ~1024). The read side is still unbuffered (R12-P1 above)
  — net ceiling is still bounded by the read syscalls.
- **R12-I1**: wake-path SANDBOX_TASK_DRIVER consultation is a single
  `std::env::var()` in `task_driver_mode_from_env` (sub-µs;
  in-process env table read), plus 14 additional small `to_string()`
  /`display().to_string()` calls in the ChPlugin branch of
  `build_restore_nomad_job_json`. Per-restore impact: <1 µs +
  ~14 heap allocations of <64 bytes each. **No measurable shift
  in the wake-path budget.**

| Component | Estimate | Source |
|---|---|---|
| `submit_restore_job` (spawn_blocking) | ~2.0-3.5 s | `restore_handler.rs:508-524` + `:1450-1508` |
| `wait_for_livez` (spawn_blocking) | ~1.0-3.0 s | `restore_handler.rs:533-542` + `:1518-1537` |
| `store.get` AEAD-active path (hard-link to stage + 1 GB decrypt-write to target) | ~1.0-2.0 s | `snapshot_aead.rs:633-678` (R9-P1) |
| GCS download (L1 miss path; write side now buffered) | ~0.8-2.0 s | `snapshot_store_gcs.rs:438-458` (R11-P2 closed; R12-P1 read-side open) |
| `clock_resync` (spawn_blocking; one /dev/urandom read + one signed POST) | ~0.05-0.15 s | `restore_handler.rs:1582-1668` |
| `register_restored` + state.write insert | <0.01 s | `nomad_ch.rs:1659-1690` |
| pg awaits (5 fresh handshakes per R11-P1) | ~0.05-0.2 s | R11-P1 OPEN |
| Connection-setup overhead (~32 fresh ureq calls × 1-3 ms TCP 3WHS) | ~0.03-0.10 s | R10-P6 OPEN |
| **Post-store.get diagnostic** (3 sync `std::fs::metadata` + 3 `format!` on ntex worker thread, not spawn_blocking; bug-#14a artifact at `restore_handler.rs:443-458`) | ~0.0001-0.003 s | new (NOT a finding — too small to flag, but the only sync filesystem syscall remaining on the wake-path ntex worker thread post-store.get) |

**Estimated wake p50 (AEAD active, L1 hit)**: **~4.1-9.0 s**. Same
range as r11 — none of the new closures or r12 findings move the
median by an estimable amount. R12-P1's ~200-400 ms ceiling is a tail
/ L1-miss-only lever, not p50.

The R9-P1 fix remains the only single edit that could meaningfully
move the AEAD-active p50 baseline into the 3.5-7.0 s range.

## Cluster smoke retry outcome (no new wake timings)

T-8b-smoke-retry (commit `b6a99fb9`, 2026-05-25 17:43 PT) — **FAIL**:

- Provision succeeded (105 s, plugin v2 loaded as `Healthy=true`).
- CREATE failed in ~1 ms: nomad-driver-ch.v2's `StartTask` spawns
  `cloud-hypervisor --config <json>`, but CH v51.1 has no `--config`
  flag. Three controller retries → 503 to client. **Bug C-1**.
- SNAP and WAKE were never reached. No single-cycle timings.

**Expected vs actual delta**: cannot compute — no actual wake-path
sample. The expected delta from R11-P2 closure was a small p50 / tail
shrink on L1-miss wakes (write-side syscall collapse). Whether the
read-side R12-P1 needs to land before this is observable is itself
unmeasurable until a cluster cycle goes end-to-end.

NO-GO for T-8b-stress (per the cluster review) until nomad-driver-ch
v3 lands the StartTask fix.

## `compio::runtime::spawn` audit (re-run from r10)

Scanned `crates/sandbox/src/**/*.rs` with `grep -n
"compio::runtime::spawn("`. Eleven match sites:

| Site | Body | Verdict |
|---|---|---|
| `main.rs:115` | `preview_ws::serve(...)` (long-lived async server) | OK — pure async |
| `lib.rs:989` `start_health_loop` | loop of `compio::time::sleep` + `state.backend.probe().await` | OK — pure async |
| `lib.rs:1072` `spawn_heartbeat_task` | loop with `db.heartbeat().await` + `compio::time::sleep` | OK — pure async (pg work is async via Pool) |
| `lib.rs:1283` `spawn_takeover_task` | loop with `compio::time::sleep` + db pg awaits | OK — pure async |
| `lib.rs:2145` | test-only fixture loop | OK — test code |
| `sweep.rs:227` `spawn_transient_state_takeover` | loop with `compio::time::sleep` + `run_transient_takeover_once(...).await` | OK — pure async (the inner work is async; the per-pool churn is R11-P1, not a spawn-misuse issue) |
| `sweep.rs:563` `spawn_idle_snapshot_sweep` | loop with `compio::time::sleep` + `run_idle_eviction_once(...).await` | OK — pure async |
| `registry.rs:829` `start_idle_gc` | loop with `compio::time::sleep` + `catch_unwind` on a `sandboxes.expired()` walk (in-mem, sync, ~µs) + async `backend.stop` | OK — the sync `expired()` is in-mem hashmap walk under a Mutex; sub-µs; not heavy enough to need spawn_blocking |
| `admin_handlers.rs:1311` | detached `state.backend.teardown_source_for_snapshot(...).await` | OK — async work, fire-and-forget by design |
| `backend/nomad_ch.rs:2002` `guard_detached("nomad_ch_create_guard_cleanup", ...)` | async cleanup work in Drop guard | OK — pure async |

**Result**: no new misuse. All sites that do sync I/O on the wake/
snapshot critical path go through `compio::runtime::spawn_blocking`
(audited individually in R10/R11 and now untouched by r12 commits).

## Allocation-heavy paths added in R12-I1

`build_restore_nomad_job_json` under the new `TaskDriverMode::ChPlugin`
arm (`restore_handler.rs:1378-1418`) constructs a `serde_json::json!`
block with ~13 owned strings: `kernel_path.display().to_string()`,
`alloc_dir.display().to_string()`, `sandbox_id.simple().to_string()`,
two image-path `.display().to_string()`s, plus the static-string
literal fields. Per-restore total: ~14 short `String` allocations
(<64 bytes each) + the outer `serde_json::Value` tree. **Per-restore
cost: <50 µs** at typical jemalloc latency. Not material against the
wake-path budget; not a finding.

The RawExec arm under R12-I1 reduced its surface area: only
`cfg.wrapper_path.display().to_string()` in the Config, with the rest
of the per-VM data riding in `Env` (unchanged). No regression.

## Carry-forward (still open from r11 / r10 / r9)

| Finding | Status | File:line |
|---|---|---|
| **R11-P1** Every `Database` method opens fresh pg Pool         | OPEN | `db.rs:492-516`; arch-confirmed thread-local recipe (this review) |
| **R10-P1** AEAD-active snapshot reads memory-ranges 5×         | OPEN | `snapshot_aead.rs:377-465`, `snapshot_store.rs:184-223`, `snapshot_store_gcs.rs:540-622, 966-997` |
| **R10-P3** `cipher.encrypt`/`decrypt` allocates fresh Vec/chunk | OPEN | `snapshot_aead.rs:411-446, 529-572` |
| **R10-P4** `GcsSnapshotStore::put` recomputes canonical SHA    | OPEN | `snapshot_store_gcs.rs:559-560, 1066` |
| **R10-P5** AEAD encrypt + decrypt write to raw `File` (no `BufWriter`) | OPEN | `snapshot_aead.rs:403, 529` |
| **R10-P6** ~32 fresh `ureq` connections per wake (no pooled `Agent`) | OPEN | `restore_handler.rs:1465-1490` |
| **R10-P7** `clock_resync_random_hex` builds via `format!("{b:02x}")` loop | OPEN | `restore_handler.rs:1687-1772` |
| **R9-P1** AEAD-active wake-path get discards R5-P1 hard-link  | OPEN | `snapshot_aead.rs:633-678` (lines 660-674: full plaintext copy to target) |
| **R9-P3** AEAD 3-pass fusable on snapshot put                 | OPEN (subsumed by R10-P1) | — |
| **R9-#6** 64 KiB scratch buffer inside `ARTIFACT_FILES` loop  | OPEN | `snapshot_store.rs:207`, `snapshot_store_gcs.rs:509, 981` |
| **R9-#8** `chunk_aad` allocates a 13-byte Vec per chunk       | OPEN | `snapshot_aead.rs:325-330` |
| **R11-P4** Sweep `SandboxRow::clone` allocation profile        | OPEN | `sweep.rs:494-526` |

## Closed since r11

- **R11-P2 + R11-P3**: BufWriter on GCS `download_to_disk` + BufReader
  on `canonical_artifact_sha256` + `sha256_file` — landed at
  `3d5c527f` (`crates/sandbox/src/snapshot_store_gcs.rs`). Confirmed
  on disk. **(R11-P2 left the read side unbuffered — see R12-P1 above
  for the symmetric follow-up.)**

## Ranked next-biggest perf lever (updated for r12)

1. **Hoist `Database` Pool to per-thread `Rc<Pool>`** (R11-P1):
   code-derived 4× PG handshake savings per wake, larger on every
   other db method + sweep loops. Arch-confirmed shape; the largest
   non-AEAD low-effort lever.
2. **Eliminate the second 1 GB write on AEAD-active wake** (R9-P1):
   ~0.5-1.5 s/wake saved; pairs with the c=N SSD-contention concern.
3. **Fuse encrypt + canonical-SHA + L2-side SHA into the streaming
   pipes** (R10-P1 + R10-P4 + r9 #2): ~1.5-2.5 s/snapshot saved at
   c=N stress.
4. **`encrypt_in_place_detached` + `decrypt_in_place_detached`**
   (R10-P3): plausible 100-400 ms per AEAD round-trip;
   measurement-dependent.
5. **Cached `ureq::Agent`** (R10-P6 / r9 #3): ~30-100 ms/wake.
6. **BufReader on GCS download read side** (R12-P1, NEW): ~200-400 ms
   syscall-overhead ceiling on L1-miss wakes; pairs with R11-P2's
   write side already landed.
7. **`BufWriter` on AEAD encrypt/decrypt** (R10-P5): ~200-400 ms
   ceiling per AEAD pass; same shape as R11-P2 but on the local
   files.
8. **Sweep allocation cleanup** (R11-P4): sub-ms; code-quality
   more than perf.

## Notes on focus-area questions

**(R11-P1 thread-local feasibility)**: confirmed at `compio-postgres/
src/pool.rs:14-17` (Pool `!Send` because compio's TcpStream uses Rc
internally) and `:341` (`start_housekeeper` takes `&Rc<Self>`). The
architecture-r11 sketch's per-worker `thread_local!<RefCell<Option<
Rc<Pool>>>>` is the only viable shape — covered above.

**(Wake-path latency budget update)**: closures since r11 (R11-P2 +
R11-P3) shrink the L1-miss tail's syscall-overhead ceiling by ~50%
(write side); the read side (R12-P1) remains for the symmetric win.
R12-I1 introduces no perf delta. p50 unchanged at ~4.1-9.0 s
(AEAD-active, L1 hit).

**(Cluster smoke retry)**: FAILED at CREATE (Bug C-1) — driver-side
fix needed before any wake/snapshot data can be collected. The r11
estimate cannot be reconciled against measurement until at least one
end-to-end cycle lands.

**(spawn-not-spawn_blocking audit)**: 11 sites scanned; zero
violations. All wake-/snapshot-path sync I/O still routes through
`compio::runtime::spawn_blocking` correctly.

**(R9-P1 status under AEAD-active wake)**: confirmed open at
`snapshot_aead.rs:660-674`. The wake `get()` stages to `<target>.
aead-stage`, then `decrypt_to(&wrapped, &plain, ...)` does a fresh
~1 GB ciphertext-read + plaintext-write to the target. With
R10-P2's `spawn_blocking` wrapper landed and R9-P1 still open, the
AEAD-active wake still pays the full second 1 GB write on the hot
path — the single largest single-edit lever.

**(Allocation sweep in R12-I1)**: 14 short String allocations per
restore. Sub-µs total. Not a finding.

**(Post-store.get diagnostic on ntex thread)**: `restore_handler.rs:
443-458` does 3 sync `std::fs::metadata` calls + 3 `format!` builds
+ `Vec::join(", ")` on the ntex worker thread (not spawn_blocking),
between store.get's spawn_blocking and the rewrite_config_json call.
Bug-#14a diagnostic, retained post-investigation. Cost: ~100 µs warm
cache, up to ~3 ms cold. Documented in the latency table; below the
flagging threshold.
