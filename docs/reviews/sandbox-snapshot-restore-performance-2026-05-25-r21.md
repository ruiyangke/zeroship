# Sandbox/snapshot-restore — performance r21 review

Date: 2026-05-25 (UTC).
HEAD at audit: `8bc11768` (branch `feat/sandbox-snapshot-restore`).
Prior: `…performance-2026-05-25-r20.md`.

Round 21 evaluates: r17-Q3 (`17d65f83`), R10-API4+R12-API1 readyz
envelope (`528c3c44`, `21788c4a`), r21-A1 restore-path `user_id`
(`fcac5355` + driver pin v8 `4d73a5d1`), smoke-r19's WAKE 109.7 s
decomposition (THEORY A confirmed; new CH `CreateConsoleDevice`
failure), concurrency-r21's R21-I1 watchdog proposal, and the
Phase-3 carry queue.

## Summary

**5 findings, 0 CRITICAL, 1 IMPORTANT, 3 MINOR, 1 INFO.** Nothing
landed since r20 touches the SLO floor:
- **r17-Q3** swaps `unwrap_or(Failed)` for explicit `Err(DataIntegrity)`
  (`db.rs:1625-1658`). +1 owned-String alloc per row read (~50-100 ns,
  far below noise). Migration-0009 CHECK keeps the Err branch
  unreachable. Perf-neutral.
- **R10-API4 / R12-API1** rewrite readyz error body via §10.0
  envelope (`handlers.rs:131-145`). Operator endpoint, not on WAKE
  path. Perf-neutral.
- **r21-A1** adds one JSON field to the restore-path builder
  (`restore_handler.rs:2349-2357`) inside the existing
  `spawn_blocking` hop. Sub-microsecond.

**Smoke-r19 confirms r20's prediction**: 60 s ch.sock probe budget
IS the new unhappy-path WAKE floor. 109.7 s wall decomposes as
vm_index race (~32 s at attempt 17) + Restoring phase work (~17.5 s)
+ ch.sock probe (60 s/599 attempts) + nomad propagation (~0.5 s).
**C-7-LT-9 (serial.file retarget+touch) is the last bug between us
and the first end-to-end green WAKE.** Projected post-fix WAKE p50:
**~10-15 s warm / ~40-60 s cold-cache+source-race** (see R21-P1).

**Phase 3 queue unchanged:** R16-P3 → R16-P5 → R11-P1 → R17-P2. Only
NEW perf-relevant item: **R21-M1** (Restoring watchdog tick, 2-4
extra UPDATEs over a slow restore, ~5-15 ms total — well below SLO
floor).

## CRITICAL

None.

## IMPORTANT

### R21-P1 — projected post-C-7-LT-9 WAKE p50 ~10-15 s; reproducible-cluster decomposition

**File:Line:** smoke-r19 cluster log `state machine transitions`
(`+0.578s`, `+32.220s`, `+109.714s`) +
`wake_machine.rs:244-444` (state-machine phase ordering) +
`restore_handler.rs:2349-2357` (restore-path builder) + driver
`startTaskRestoreBranch` 60 s probe (already documented in r20-M3).

**Smoke-r19 verbatim decomposition** (109.7 s wall):

| Stage | Wall | Source / floor |
|---|---|---|
| POST → 202 mint | 58 ms | controller (idempotency precheck + insert + spawn) |
| `pending → reserving_slot` | ~520 ms | controller |
| `reserve_vm_index_with_retry` (attempt 17 / 16 × 2 s + source-fence at +300 ms) | 31.6 s | `restore_handler.rs::reserve_vm_index_with_retry`; capped by `host_fence_timeout(30, async)`. Floor: **source-teardown race, ~30 s.** |
| Restoring phase (alloc_dir teardown + `store.get` + `rewrite_config_json` + `submit_restore_job`) | ~17.5 s | `wake_machine.rs:272-362`. `store.get` is 1 GB GCS read + AEAD decrypt; the post-C-7-LT-9 happy path keeps this. Floor: **~10-25 s cold-cache, ~3-8 s warm.** |
| ch.sock probe (60 s budget, 599 attempts) | 60.0 s | driver v8 `startTaskRestoreBranch`. **Floor on unhappy path; collapses to ~100-300 ms on happy path.** |
| Nomad terminal propagation | ~0.5 s | nomad alloc → controller poll. |
| **Total (smoke-r19 RED)** | **109.7 s** | |

**Projection once C-7-LT-9 lands** (driver retargets+touches
`serial.file` before `cloud-hypervisor --restore`, CH binds ch.sock
within sub-100 ms):

| Stage | Projected p50 | Notes |
|---|---|---|
| POST → 202 mint | 58 ms | unchanged |
| `pending → reserving_slot` | ~520 ms | unchanged |
| `reserve_vm_index_with_retry` | **~2 s warm / ~30 s if source-teardown raced** | the smoke-r19 race-resolution is structural; on a freshly-provisioned snapshot with no live source, this is ~2 s |
| Restoring (store.get + config + submit) | ~3-8 s warm GCS / ~10-25 s cold | dominated by 1 GB AEAD decrypt — **R16-P3/P5 target this** |
| ch.sock probe | **~100-300 ms** | 1-3 attempts at 100 ms cadence |
| LivezPolling (`wait_for_livez`) | ~5-15 s | first production exercise of R19-I1 two-phase probe; bounded by `agent_livez_timeout_secs` ≈ 30 s |
| ClockResyncing + Registering | sub-second each | |
| **Projected WAKE p50 (warm, no source-race)** | **~10-15 s** | |
| **Projected WAKE p50 (cold-cache, source-race)** | **~40-60 s** | |
| **Projected WAKE p99 (unhappy)** | **~30-50 s** (matches r20 prediction) | source-teardown 30 s + reserve ~2 s + restore ~10-25 s + boot ~5-15 s |

**The single largest remaining lever post-C-7-LT-9 is R16-P3 + R16-P5**
(fused AEAD+SHA, gzip pre-AEAD) — these compress the Restoring
phase's 1 GB AEAD-decrypt window from ~10-25 s to ~3-8 s. Net
expected WAKE p50 after Phase-3 lands: **~5-10 s warm, ~15-25 s
cold**, matching the r19/r20 standing roadmap.

**Action**: no roadmap change. Validate the projection at smoke-r20+
(first cycle past C-7-LT-9 — exercises R19-I1 two-phase livez probe
for the FIRST time, and Phase-3 wins become measurable). The
`attempts=N` probe metric (driver task event, plumbed since r20-M3)
is the key smoke signal — verify `attempts < 10` post-fix.

## MINOR

### R21-M1 — R21-I1 watchdog tick: 5-15 ms total per slow-restore, well below SLO floor

**File:Line:** `wake_machine.rs:272-362` (Restoring phase) +
`db.rs:3192-3232` (`update_wake_job_state`). Sweep cadence
(`sweep.rs:367`) = 60 s; default threshold (`config.rs:1029`) = 60 s.

The concurrency-r21 proposal: spawn a 20-30 s tick inside Restoring
that calls `update_wake_job_state(Restoring, None, None, None)` to
bump `lessee_updated_at`. Per-tick cost: `pool.get()` ~100-500 µs +
indexed UPDATE on `wake_id = $5` PK (~1-3 ms) + ~sub-ms tracing =
**~2-5 ms/tick**. A 60-90 s Restoring with 20-30 s cadence runs **2-4
ticks = ~5-15 ms total**, ~0.1% of even the optimistic 10 s WAKE p50.
R11-P1 (per-thread pg pool) would shave per-tick to ~1-3 ms but the
bound stays trivially small either way.

Two implementation notes (no perf objection to either): (1) the
watchdog must not block on `spawn_blocking` hops — it needs its own
spawned async task; (2) at 20-30 s tick cadence + 60 s threshold, the
`lessee_updated_at` bump is always < 30 s old when sweep checks, so
no false-takeover race remains.

**Verdict:** Perf-neutral. Add as Tier-2.5b.

### R21-M2 — r17-Q3 DataIntegrity Err path: ~50-100 ns/row owned-String alloc; unmeasurable

**File:Line:** `db.rs:1625-1658`. **Diff:** `17d65f83`.

Before: `r.get("state")` returned `&str` + `unwrap_or(Failed)`. After:
`r.try_get("state")?` returns owned `String` + explicit `Err` branch.
Net delta: **+1 short-String alloc (~30-50 bytes, ~50-100 ns)** per
`get_wake_job` / `find_pending_wake_for_sandbox` row. At ~10 polls/wake
this is ~500-1000 ns/wake total, ~0.03% of one pg round-trip —
unmeasurable. The `.transpose()` at `db.rs:3167/3266` is pure stack
manipulation; the retry loop's `?` short-circuit costs nothing on
happy path. **Perf-neutral.** A micro-opt back to `&str` would save
~50 ns but lose the drift-detection surface — not worth it.

### R21-M3 — R10-API4 readyz envelope: perf-neutral (operator endpoint)

**File:Line:** `handlers.rs:131-145`. **Diff:** `528c3c44`. The 503
path now goes through `ErrorEnvelope::new()` — one extra struct
construction (~100 ns) on the unhealthy branch, zero on healthy.
`readyz` is the operator healthcheck, not on WAKE path. Perf-neutral.

## INFO

### R21-I1 — r21-A1 user_id emission: sub-microsecond JSON field add

**File:Line:** `restore_handler.rs:2349-2357`. **Diff:** `fcac5355`.
One `serde_json::Value` key-value pair added to a Config block that
already serialises ~15 fields, inside the existing `spawn_blocking`
hop at `wake_machine.rs:351`. ~50-200 ns marginal cost on a ~17.5 s
Restoring phase = sub-microsecond. Perf-neutral. The two-emitter
debt (cold-boot + restore both emit the same fields) is a
code-quality concern, not a perf one.

## Quantified-win roadmap (carry from r20; one new Tier-2.5 candidate)

| Finding | File:Line | Win | Effort | Risk |
|---|---|---|---|---|
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | **~3-4 s/SNAPSHOT** (r15/r19 14.5 s constant) | ~80 LOC | medium |
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | **SNAPSHOT ~3.5 s + WAKE ~2 s** + 95% storage | ~200 LOC | medium |
| **R11-P1** Per-thread pg pool | `db.rs:515-521` | ~400-600 ms/wake controller CPU + SLO floor (also benefits R21-I1 watchdog) | ~150 LOC | medium |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC | low |
| **R17-P1** Skip intermediate set_state writes | `wake_machine.rs:358, 381, 443-456, 128` | ~4-10 ms/wake (note: collides with R21-I1 if both land — review interaction) | ~30 LOC | medium |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs | ~25 LOC | low |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:185`, `admin_handlers.rs:1585-1604` | ~2-6 ms/wake | ~15 LOC | ~zero |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms | 2 LOC | ~zero |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:288-348` | instrumentation | ~5 LOC | ~zero |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s background (moot post-R16-P5) | 1 LOC | ~zero |
| **R21-M1** Restoring-phase watchdog tick (NEW, from concurrency-r21 R21-I1) | `wake_machine.rs:272`, `db.rs:3192` | concurrency fix; ~5-15 ms wake cost (perf-neutral) | ~30 LOC | low |
| **R17-P3 / R17-P4 / R18-M2 / R16-P8** trivia/instrumentation | (various) | ~zero–polish | small | ~zero |
| ~~R16-P2~~ livez 500 → 200 ms | `nomad_ch.rs:3091` | n/a | **CLOSED (R19-I1)** | |

**Top-3 SLO wins (unchanged):** R16-P3 (~3-4 s/SNAPSHOT) → R16-P5
(~3.5 s SNAPSHOT + 2 s WAKE) → R11-P1 (~400-600 ms wake-POST/poll).

**Carry interaction note (R17-P1 ↔ R21-M1):** R17-P1 proposed
*removing* intermediate `set_state` writes to shave ~4-10 ms/wake.
R21-M1 proposes *adding* a periodic `update_wake_job_state` watchdog
tick inside Restoring. **These are not directly contradictory** —
R17-P1 elides redundant writes at phase boundaries; R21-M1 adds a
deliberate keep-alive *within* a long phase. Both can land. But the
final wake-machine should be reviewed in one PR so the lessee-renewal
contract is documented coherently (every Restoring-phase tick OR
every phase boundary write keeps the lease fresh; never both
simultaneously for the same transition).

## Cross-lens consensus

- **Concurrency r21 (R21-I1 Restoring-phase watchdog)**: perf-lens
  concurs the watchdog tick is structurally cheap (~5-15 ms over a
  slow restore). Recommend coupling the implementation with R17-P1
  so the lessee-renewal contract is re-documented as a single
  invariant. No perf objection to either landing first.
- **Concurrency r21 (R20-C1 terminal write CAS guard, still OPEN)**:
  one-line SQL `AND lessee = $host` predicate on
  `update_wake_job_state`. Adds one extra index probe per
  state-transition write (~50-100 µs). Wake machine has ~6
  state-transitions, so ~300-600 µs/wake — well below SLO floor.
  **No perf objection.**
- **Test-coverage r21 / arch r21**: no perf-relevant items.
- **Driver-side C-7-LT-9** (next fix): once the rewriter retargets
  `serial.file` + touches it before `cloud-hypervisor --restore`, the
  60 s ch.sock probe collapses to ~100-300 ms. **This is the single
  largest WAKE win available right now (~60 s → ~0.3 s on the
  unhappy-path floor, ~99% of WAKE wall).** No Rust roadmap entry —
  lives in the driver repo.

## Lens hand-off

**Tier 2 (CLOSED):** R16-P1 + R16-P2 — unchanged from r20.

**Tier 2.5 (1 PR, ~10 ms/wake + ~5-15 ms/wake POST, low risk):**
R17-P1c + R17-P3 + R17-P4 + R17-P2 + R18-P1 + R18-P2 metric.
Unchanged from r18/r19/r20.

**Tier 2.5b (1 PR, R21-M1 watchdog tick; ~5-15 ms/wake, low risk):**
NEW from this round. Couple with R17-P1 review.

**Tier 2.75 (1 PR, R11-P1; ~400-600 ms controller CPU + SLO floor,
medium risk):** unchanged.

**Tier 3 (2 PR, ~6-7 s SLO win, medium risk):**
- **R16-P3 fused encrypt + SHA (PR-a)** — Tier-3 lead. r15+r19
  reproducible ~14.5 s constant confirms the SLO target.
- **R16-P5 gzip-before-encrypt (PR-b)** — gates on PR-a's chunk
  loop.

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT. Unchanged.

R16-P4 + R16-P7 + R17-P1a/b defer until Tier-3 lands.

## Carry-forward (r20)

All r20 items remain OPEN with no priority change. R20-M3 (60 s
ch.sock probe budget) is being **actively closed by driver-side
C-7-LT-9** — smoke-r20 (first cycle past C-7-LT-9) will measure the
new unhappy-path WAKE floor and let perf-lens collapse the carry.

## Closures since r20

- **R10-API4 + R12-API1** (readyz envelope) — LANDED at `528c3c44` +
  `21788c4a`. Perf-neutral (operator endpoint).
- **r17-Q3** (`wake_job_row_from_pg` returns Err on drift) — LANDED
  at `17d65f83`. Perf-neutral on happy path; +1 String alloc per
  row read (~50-100 ns, far below noise).
- **r21-A1** (`user_id` in restore-path builder) — LANDED at
  `fcac5355` + driver pin `4d73a5d1`. Perf-neutral (sub-microsecond
  JSON field add inside existing `spawn_blocking` hop).

## Focus-area notes (brief)

**Post-C-7-LT-9 SLO projection** (smoke-r20, driver vN+1 +
serial.file retarget+touch):
- CREATE: ~4.5-5.1 s (R19-I1 already in place).
- SNAPSHOT: ~14.5 s (unchanged; AEAD-decrypt-dominated).
- **WAKE happy (warm, no race): ~10-15 s.**
- **WAKE unhappy (cold-cache + source-race): ~40-60 s.**
- **WAKE p50 (mixed traffic, post-C-7-LT-9, pre-Phase-3): ~15-25 s.**

**Tier-3 stacked projection** (R16-P3 + R16-P5 landed on top of
C-7-LT-9):
- SNAPSHOT 14.5 → ~8-9 s.
- WAKE happy 10-15 → ~5-10 s.
- WAKE p50 mixed → ~10-15 s.

**The next 3 measurement gates:**
1. Smoke-r20 WAKE wall (validates the ~10-15 s happy projection and
   exercises R19-I1 two-phase livez for the FIRST time in 7 cycles).
2. Smoke-r21+ p50 over ≥10 wakes (validates the WAKE p50 mixed
   projection; needed before Phase-3 lands so we have a true
   pre-Phase-3 baseline).
3. Phase-3 (R16-P3 + R16-P5) smoke (validates the ~5-10 s warm and
   ~8-9 s SNAPSHOT projections).
