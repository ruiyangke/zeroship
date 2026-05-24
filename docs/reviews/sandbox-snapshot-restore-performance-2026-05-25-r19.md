# Sandbox/snapshot-restore — performance r19 review

Date: 2026-05-25 (UTC)
HEAD at audit: `bffa6f1d`. Last perf round: r18 (`370d13e6`).
This round evaluates R19-C1 (wake_jobs takeover sweep), R19-I1
(two-phase agent /livez probe), and re-examines smoke-r14 actuals
(CREATE 6.46 s / SNAPSHOT 14.72 s) — particularly the SNAPSHOT
"regression" from r11's 6.3 s.

## Summary

**5 new findings, 1 IMPORTANT, 3 MINOR, 1 root-cause INFO.** R19-C1's
takeover sweep is ~0 cost on the happy path (one indexed UPDATE
against the partial `wake_jobs_lessee_idx`, fires once per 60 s).
R19-I1's two-phase probe is ~0 cost on the happy path (loopback TCP
connect <1 ms) and a **net win** on the unhealthy path (caps SYN
retransmit at 150 ms instead of ~30 s). The SNAPSHOT 14.72 s
"regression" from r11's 6.3 s is **not a regression**: r11 ran with
`SANDBOX_SNAPSHOT_ROOT_KEK_PATH` UNSET → `AeadSnapshotStore` in
passthrough mode → `encrypt_in_place` was a no-op. r12+ runs AEAD
end-to-end (read 1 GB plaintext + ChaCha20-Poly1305 + write 1 GB
ciphertext + re-read 1 GB for canonical SHA). The 8.4 s gap is real
AEAD critical-path work, which **promotes R16-P3 (fused AEAD+SHA) to
top SLO win** — it cuts ~1.5-2 s out of the 14.7 s. R16-P2 (livez
500→200 ms ureq timeout) is **now obsoleted** by R19-I1: Phase 1's
150 ms TCP-connect gate fires first, so Phase 2's 500 ms ureq budget
is only reached when TCP is already ACKing — i.e. when 500 ms is
not the floor.

Phase 3 roadmap re-priorities: **R16-P3 → R16-P5 → R11-P1**. R17-P2
DashMap cache still TODO. R16-P2 closed (obsoleted).

## CRITICAL

None.

## IMPORTANT

### R19-P1 — SNAPSHOT 14.72 s is AEAD critical-path; R16-P3 cuts ~1.5-2 s, R16-P5 cuts ~3.5 s

**File:Line:** `crates/sandbox/src/snapshot_aead.rs:377-453`
(`encrypt_in_place`), `crates/sandbox/src/snapshot_store.rs:184-223`
(`compute_artifact_sha256`), `crates/sandbox/src/snapshot_handler.rs:381-407`
(critical-path `put` call).

**Root-cause re-trace.** Smoke-r11 had `snapshot p50=6314 ms` because
`SANDBOX_SNAPSHOT_ROOT_KEK_PATH` was unset → `RootKek::from_env()` →
`Ok(None)` → `AeadSnapshotStore::root = None` → `encrypt_in_place`
returns `Ok(())` on line 384 without touching `memory-ranges`. Smoke
r12 hot-patched the KEK on the running worker mid-cycle (cluster doc
r12 § "Validation 1": "added `$ART/snapshot-root-kek` (32 bytes,
0o400) + env. The fail-CLOSED boot assertion landed since v26 caught
this"). Every subsequent smoke (r12, r13, r14) runs AEAD
end-to-end, and the SNAPSHOT wall jumped from 6.3 s → 14.6-14.7 s
across all three.

**r14 SNAPSHOT 14.72 s breakdown** (derived from code shape on
`bffa6f1d` against ~1 GB `memory-ranges`):

| Stage | Wall | Source |
|---|---|---|
| `ch.snapshot` (CH → temp dir, ~1 GB I/O) | ~2.1 s | snapshot_handler.rs:373-378 (unchanged since r11) |
| `encrypt_in_place` (read 1 GB plaintext + ChaCha20-Poly1305 + write 1 GB ciphertext) | ~5-6 s | snapshot_aead.rs:411-453 (loop over `CHUNK_PLAINTEXT_LEN` chunks) |
| `compute_artifact_sha256` (re-read 1 GB ciphertext through `BufReader<1 MiB>` + Sha256) | ~3-4 s | snapshot_store.rs:184-223 |
| rename to canonical L1 path + meta writes | ~1 s | snapshot_store.rs:225+ |
| **Total** | **~11-13 s** + ~2 s jitter | matches observed 14.72 s |

**The 8.4 s delta r11 → r14 is real AEAD critical-path work**, not
GCS jitter (the r12 review's stated cause is incorrect — L2 GCS is
fire-and-forget on a detached thread per `snapshot_store_gcs.rs:1143-1169`
`detach_isolated("snap-l2-upload-<tail>")`, so it cannot gate the
SNAPSHOT RPC return).

**Implications for the Tier-3 roadmap:**

1. **R16-P3 (fused AEAD+SHA)** is no longer a "nice to have ~1.5 s" —
   it eliminates the entire 3-4 s `compute_artifact_sha256` re-read by
   feeding each `encrypt_in_place` ciphertext chunk into a `Sha256` as
   it's written. Estimated win: **~3-4 s** off the 14.7 s SNAPSHOT
   (revised up from r16's "~1.0-2.0 s" because the AEAD path is now
   the dominant fraction).
2. **R16-P5 (gzip memory-ranges pre-AEAD)** still ~3.5 s; the
   compressible plaintext shrinks the AEAD input + the canonical-SHA
   input + the L2 upload bytes proportionally.
3. **Stacked**: R16-P3 + R16-P5 takes SNAPSHOT 14.7 → ~8-9 s.

**Action**: re-rank Tier-3 to land **R16-P3 first** (smaller diff,
larger SLO win, prereq for R16-P5's chunk-loop). R16-P5 follows as
PR-b.

## MINOR

### R19-M1 — R19-C1 takeover sweep is ~0 ms/tick (indexed partial UPDATE)

**File:Line:** `crates/sandbox/src/db.rs:3289-3320`
(`claim_orphan_wake_for_recovery`), `crates/sandbox/migrations/0010_wake_jobs_hardening.sql:48-50`
(`wake_jobs_lessee_idx`), `crates/sandbox/src/sweep.rs:436-463`
(60 s cadence).

The sweep's UPDATE predicate `(state NOT IN ('ok','failed') AND
lessee_updated_at < now() - interval)` is fully covered by the
partial btree `wake_jobs_lessee_idx WHERE state NOT IN (...)` from
migration 0010. pg scans **only the non-terminal partition** (≈
active wakes; at c=20 stress ~20 rows; at p50 idle 0 rows).

**Cost per tick:** 1 pool.get() handshake (1-3 ms RTT, or R11-P1
amortised) + 1 indexed UPDATE (sub-ms server-side on a tiny
partition, returns 0 rows on a healthy fleet). **Steady-state
overhead**: ~2-5 ms / 60 s = **0.005-0.008% of controller pg time**.
Pathological burst (N=100 orphaned at once) ≈ 5-10 ms server-side.
Bounded.

**Optimisation (not recommended)**: `LIMIT N` on UPDATE (same shape
as R18-M2 for GC) would future-proof at 1000× scale. Defer — sweep
is bounded by the partial index already. INFO.

### R19-M2 — R19-I1 two-phase probe is ~0 cost on happy path, big win on unhealthy

**File:Line:** `crates/sandbox/src/backend/nomad_ch.rs:3091-3213`
(`wait_for_agent_livez`), `:3477-3487` (`probe_agent_reachable_tcp`).

**Per-iteration cost on the happy path (agent live)**: Phase 1
loopback TCP-connect <1 ms + Phase 2 ureq /livez ~1-2 ms + (if 200)
signed /version ~3-5 ms + 150 ms sleep. Total ~155-160 ms.
**No measurable difference vs pre-R19-I1 single-shot ureq.**

**Per-iteration cost on the unhealthy path**: Phase 1 caps stuck SYN
at 150 ms (io_uring CANCEL); Phase 2 skipped. Saves up to **29.85 s
per stuck probe** vs ureq's kernel-SYN-retransmit wedge.

**CREATE-path impact** (smoke-r14): create wall 6.46 s; ~4.85 s
spent in `wait_for_agent_livez` (`agent_ready elapsed_ms=5629` −
`alloc_running 784`). Pre-R19-I1 burned 1-2 stuck-SYN probes during
agent boot (TCP refused → ureq 500 ms × 2-3 probes ≈ 1.3-2.0 s
waste). Post-R19-I1 those probes return at sub-ms. **Expected
CREATE wall: 4.5-5.1 s** on smoke-r15. Verify post-cutover.

**Net**: R19-I1 **obsoletes R16-P2** — Phase 1's 150 ms TCP cap is
structurally tighter than R16-P2's proposed 200 ms ureq cap and
fires unconditionally; Phase 2's 500 ms ureq budget is only reached
on TCP-ACKed-but-HTTP-wedged (a backstop, not a floor). INFO.

### R19-M3 — R11-P1 per-thread pg pool: priority unchanged, but landing surface widens

**File:Line:** `crates/sandbox/src/db.rs:515-521` (`open_pool` minted
on every call).

R18 raised R11-P1 priority because each `wake_jobs` SELECT + INSERT
pays a 5-15 ms pool-open handshake. R19-C1 adds two more pool-open
sites (the 60 s sweep's `claim_orphan_wake_for_recovery` and the
GC's `gc_expired_wake_jobs`), but these are off the wake-POST
critical path — they don't move the SLO floor. **R11-P1's primary
target remains the wake-POST + wake-poll handshakes** (R17-P2 cache
is the complement on the poll side).

INFO. No re-prioritization. R11-P1 stays at Tier 2.75.

## INFO

### R19-I1 — root-cause of the SNAPSHOT 14.7 s "regression" (configuration delta, not code regression)

See R19-P1 above. Single-line summary: r11 ran AEAD passthrough
(no KEK env); r12+ runs AEAD end-to-end (KEK provisioned). The 8.4
s delta is the AEAD encrypt + canonical-SHA passes the
`AeadSnapshotStore` puts on the critical path when `root.is_some()`.
**No regression; the smoke-r11 6.3 s was an artifact of an
unencrypted snapshot path.** The r12 review's "GCS RTT jitter"
explanation is mistaken — L2 is detached (fire-and-forget), so GCS
cannot gate the put RPC.

Documentation fix recommended: add a one-liner to the deferred doc
noting r12+'s 14.7 s is the AEAD-enabled baseline; r11's 6.3 s is
not a comparable datapoint.

## Quantified-win roadmap (updated)

| Finding | File:Line | Win | Effort | Risk |
|---|---|---|---|---|
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | **~3-4 s/SNAPSHOT** (revised up; was 1-2 s in r16 before AEAD baseline was known) | ~80 LOC + tests | medium |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT ~3.5 s + WAKE ~2 s + 95% storage | ~200 LOC + format-rev | medium |
| **R11-P1** Per-thread pg pool (raised priority) | `db.rs:515-521` | ~400-600 ms/wake controller CPU + SLO floor | ~150 LOC (per-worker thread_local) | medium |
| **R17-P1** Skip intermediate set_state writes / batch terminal | `wake_machine.rs:358, 381, 443-456, 128` | ~4-10 ms/wake | ~30 LOC + transaction | medium |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC (DashMap on AppState) | low |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC removed | low |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:185`, `admin_handlers.rs:1585-1604` | ~2-6 ms/wake | ~15 LOC | ~zero |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms (CREATE + WAKE) | 2 LOC | ~zero |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:288-348` | instrumentation; enables fence right-sizing | ~5 LOC | ~zero |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s off L2 background (moot post-R16-P5) | 1 LOC | ~zero |
| **R17-P3** Static SQL constants | `db.rs:3030-3039` | trivial; prep-cache friendliness | ~8 LOC | ~zero |
| **R17-P4** Drop `.to_string()` cruft | `db.rs:2969, 3000, 3043` | sub-ms allocator | ~5 LOC | ~zero |
| **R18-M2** Bounded GC sweep DELETE LIMIT | `db.rs:3194-3211` | future-proof @ 10× scale | ~5 LOC | ~zero |
| **R16-P8** L1 hit/miss metrics | `snapshot_store_gcs.rs:1173-1201` | enables Tier-4 pre-warm | ~10 LOC | ~zero |
| ~~R16-P2~~ livez 500 → 200 ms | `nomad_ch.rs:3091` | ~~~900 ms WAKE~~ | n/a | **OBSOLETED by R19-I1** |

**Top-3 SLO-visible wins (re-ranked after R19-P1 root-cause):**
1. **R16-P3 fused AEAD+SHA** — ~3-4 s/SNAPSHOT (largest single win;
   ~25% off the AEAD baseline).
2. **R16-P5 gzip pre-AEAD** — ~3.5 s SNAPSHOT + 2 s WAKE + 95%
   storage (stacks with R16-P3 because gzip shrinks AEAD's input).
3. **R11-P1 per-thread pg pool** — ~400-600 ms controller CPU on
   wake-POST + wake-poll critical path.

R16-P2 drops out; R19-I1 already covers it structurally.

## Cross-lens consensus

- **Architecture / concurrency r19**: R19-C1 closes the long-standing
  "wake_jobs takeover sweep doesn't exist" gap. Perf lens agrees:
  the takeover SQL is one indexed UPDATE against `wake_jobs_lessee_idx`,
  amortised over 60 s, ~0 measurable cost.
- **API surface**: R19-I1 is internal to `wait_for_agent_livez`; no
  wire impact. Phase 2 still returns the same /livez+/version
  attestation; Phase 1 is a probe-before-probe.
- **Code quality**: R19-C1's `claim_orphan_wake_for_recovery` adds
  ~30 LOC of SQL + 4 pg-gated tests + 2 constant-pin lib tests.
  Clean, mirrors the GC sweep shape.
- **Security**: no change. `WakeWorkerAborted` is an error code,
  not a credential. Migration 0012 extends the existing CHECK
  constraint to accept the new value.

## Lens hand-off

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — R19-I1 obsoletes R16-P2;
R16-P1 remains as a ~150 ms polish but doesn't justify its own PR.

**Tier 2.5 (1 PR, ~10 ms/wake + ~5-15 ms/wake POST, low risk):**
R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2 metric.
Unchanged from r18.

**Tier 2.75 (1 PR, R11-P1; ~400-600 ms controller CPU + SLO floor,
medium risk):** unchanged.

**Tier 3 (2 PR, ~6-7 s SLO win, medium risk):**
- **R16-P3 fused encrypt + SHA (PR-a)** — **promoted to Tier-3
  lead** by R19-P1's root-cause. The fused stream feeds ciphertext
  chunks into both the AEAD output writer AND a Sha256 hasher in
  one pass, eliminating the 3-4 s re-read on the L1 put critical
  path. Test surface: `snapshot_aead.rs:875-1008` pins canonical
  SHA domain; PR-a must not move the hash.
- **R16-P5 gzip-before-encrypt (PR-b)** — gates on PR-a's chunk
  loop. Compressible plaintext → smaller ciphertext → smaller SHA
  input → smaller L2 upload bytes. Stacking R16-P5 on R16-P3
  takes SNAPSHOT 14.7 → ~8-9 s.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT. Unchanged
from r18.

R16-P4 + R16-P7 + R17-P1a/b defer until Tier-3 lands.

## Carry-forward (r18)

All r18 items remain OPEN except R16-P2 (closed by R19-I1's
structural fix). **R11-P1 priority unchanged.** R15-P1, R14-P3 stay
INFO.

## Closures since r18

- **R16-P2** (livez ureq timeout 500→200 ms): **CLOSED — OBSOLETED
  BY R19-I1.** Phase 1's 150 ms TCP-connect cap is structurally
  tighter than the proposed 200 ms ureq cap and fires unconditionally
  on every iteration; Phase 2's 500 ms ureq timeout is only reached
  on TCP-ACKed-but-HTTP-wedged, a rare shape that's correctly
  bounded by the outer `agent_livez_timeout_secs`. No PR needed.

## Focus-area notes (brief)

**(1) R19-C1 sweep cost**: indexed partial-index UPDATE × 60 s = ~0
measurable controller CPU. The `wake_jobs_lessee_idx` covers the
`(lessee_updated_at, state NOT IN terminal)` predicate, so the scan
touches only the active-wakes partition (≤ ~20 rows at c=20).

**(2) R19-I1 two-phase cost**: ~0 ms vs single-shot on the happy
path (Phase 1 loopback connect <1 ms; Phase 2 unchanged). ~29.85 s
win on the unhealthy path (capped 150 ms vs ~30 s kernel SYN
retransmit). Side effect: obsoletes R16-P2.

**(3) SNAPSHOT 14.72 s root-cause**: AEAD critical-path work (5-6 s
encrypt + 3-4 s SHA re-read), exposed only after r12 provisioned
the root KEK. r11's 6.3 s was passthrough-mode artifact. The 8.4 s
delta promotes R16-P3 to top SLO win.

**(4) Post-cutover SLO projection** (smoke-r15 prediction with
R19-I1 + R19-C1, before Tier-3):
- CREATE: ~4.5-5.1 s (down from 6.46 s; the ~1.3-2.0 s saved is the
  pre-R19-I1 stuck-SYN waste during agent boot probes).
- SNAPSHOT: ~14.7 s (unchanged; AEAD path).
- WAKE happy: ~3-10 s (post-C-7-LT-3 / driver-v5; depends on fence
  wall — fence now passes at 300 ms instead of 30 s, so wake is
  bounded by the source-teardown 30 s wait when snapshot-then-wake
  races).

**(5) Tier-3 stacked projection** (R16-P3 + R16-P5 landed):
SNAPSHOT 14.7 → ~8-9 s. **WAKE 3-10 → ~1.5-7.5 s** (R16-P5 shrinks
the AEAD-decrypt input on the restore path too).
