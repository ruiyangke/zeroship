# Sandbox/snapshot-restore — performance r18 review

Date: 2026-05-25 (UTC)
HEAD at audit: `370d13e6`. Lens previously reviewed at r17 (`163724dc`).
This round evaluates GATE-C2 (partial UNIQUE INDEX + `ON CONFLICT
DO NOTHING` + race-loser SELECT) and C-7-LT-1 (async-mode budget =
2×fence + HEADROOM = 70 s at fence=30).

## Summary

**4 new findings**, none CRITICAL. GATE-C2's perf footprint is
**~0 ms on the happy path** (partial UNIQUE INDEX is one btree node
extra; `ON CONFLICT DO NOTHING` is a no-op when there's no conflict)
and **+1 pg roundtrip ONLY on the race-loser path**, which at c=20
stress fires at most once per concurrent-POST burst (and is the
race-loser, so the client is already getting back the WINNER's
wake_id — strictly better than the previous 500). C-7-LT-1 raises the
async wake budget ceiling from 50 s → 70 s; this changes the **worst
case** wall-time, not p50. The r17 Tier-3 roadmap is unchanged:
**R16-P5 gzip (3.5 s), R16-P3 fused AEAD+SHA (1.5 s), R11-P1
per-thread pg pool (400-600 ms controller CPU), R16-P2 livez timeout
(900 ms)** remain the top SLO wins and are all still TODO.

Carry-forward from r17 unchanged: R17-P1/P2/P3/P4 (PR2 polish),
R16-P1/P2/P3/P4/P5/P6/P7/P8, R15-P1, R14-P1/P3, R11-P1/P4,
R10-P1/3/4/5/6/7, R9-P1/#6/#8.

## CRITICAL

None.

## IMPORTANT

### R18-P1 — `find_pending_wake_for_sandbox` is now called twice on race-lost path

**File:Line:** `crates/sandbox/src/admin_handlers.rs:1565`
(idempotency fast-path) and `crates/sandbox/src/db.rs:3049`
(post-conflict re-read inside `insert_wake_job`).

GATE-C2 wired a race-loser branch into `insert_wake_job`: on 0 rows
affected by `ON CONFLICT DO NOTHING`, it calls
`find_pending_wake_for_sandbox` to surface the winner's row. The
handler ALSO calls `find_pending_wake_for_sandbox` *before* the
INSERT (lines 1565, as an "idempotency fast-path"). Both queries use
the same partial-index predicate. On the race-loser path that's **2
identical SELECTs back-to-back**, each carrying the R11-P1 pool-open
handshake (~5-15 ms TCP+STARTUP+auth).

- **At c=1 smoke**: irrelevant — race-loser path doesn't fire.
- **At c=20 stress**: per concurrent-POST burst, exactly N-1 callers
  hit the race-loser path; N-1 callers pay 2× the SELECT instead of
  1×. ~5-15 ms × (N-1) wasted on the controller CPU. Negligible vs
  the wake SLO, but quantifiable cost.
- **Real shape**: by the time `insert_wake_job` reaches the
  post-conflict SELECT, we already know the winning row exists (the
  unique index rejected our INSERT). The handler's pre-INSERT SELECT
  *already returned None* (else we'd have replayed at line 1566). So
  the only window where both SELECTs are NEEDED is when a concurrent
  POST landed in the microseconds *between* the handler SELECT and
  our INSERT — exactly the TOCTOU GATE-C2 fixes.

**Optimisation**: drop the pre-INSERT `find_pending_wake_for_sandbox`
in `admin_handlers.rs:1565`. It's no longer load-bearing for
correctness (GATE-C2 owns that at the unique-index layer). It now
serves only as an "avoid a write roundtrip when row obviously
exists" optimisation — but it costs a SELECT *every wake POST*
(c=20 stress: 20 SELECTs/wake-burst, 19 of which are read-write
deadweight under the actual c=20 contention pattern where the
INSERT either succeeds or replays). The INSERT is single-roundtrip;
removing the precheck simplifies wake-POST to:

```
POST /wake → 1× get_sandbox_row + 1× insert_wake_job
            → Inserted: spawn machine + 202
            → Replay:   202 with winner's wake_id (re-read paid by db layer)
```

**Win**: ~5-15 ms × 100% of wake POSTs (every POST currently pays
the pre-INSERT SELECT). At ~200 wakes/s c=20 stress, that's ~1-3 s
of pg time/s eliminated from the controller. **Risk**: low; removes
~25 LOC; correctness lives in the unique index, not the precheck.

**Counterpoint**: keep the precheck if PR2's idempotency contract
("same wake POST returns same wake_id without spawning a duplicate")
is wire-stable and we don't want to depend on `insert_wake_job`'s
Replay path being hot. Then accept the 5-15 ms tax as the cost of
explicit idempotency. The comment at lines 1551-1563 already calls
it an "optimisation" rather than load-bearing — the design intent
matches "drop it".

### R18-P2 — Async budget 70 s changes worst case, not p50; needs headroom-usage metric

**File:Line:** `crates/sandbox/src/restore_handler.rs:288-348`
(`from_host_fence_timeout`), `crates/sandbox/src/restore_handler.rs:336`
(70-s ceiling at fence=30).

C-7-LT-1 raises the async-mode budget from 50 s (sync ceiling) to
70 s (2×fence + HEADROOM). Wall-time impact:

| Scenario | Sync budget | Async budget | Δ |
|---|---|---|---|
| Source already torn down (p50) | exits on first try | exits on first try | **0 s** |
| Source teardown @ 60.166 s (smoke-r12) | 503 at 50 s | 200 OK at ~62 s | +12 s wall, but now a SUCCESS |
| Source slow >70 s (pathological) | 503 at 50 s | 503 at 70 s | +20 s but still 503 |
| Wake succeeds early (no contention) | unchanged | unchanged | 0 s |

The **only path** where 70 s shows up is "source teardown is in
flight when we reserve vm_index" — which is the empirical
post-snapshot-then-wake-immediately pattern (R10-C1 / C-7 fingerprint).
The retry loop runs at 2-s intervals and exits the moment
`reserve_vm_index` returns Ok, so we do NOT burn 70 s when the
source is released early. p50 wake is unaffected.

**Gap**: **no metric for budget headroom usage**. If real workloads
land at attempt 35/36 (close to the 70-s ceiling), we want to know
*before* a pathological fence drift surfaces as 503s. The retry loop
should emit (or already does — confirm) `vm_index_retry_attempts`
histogram (or at minimum a final `attempts_used` log field on
success). Without this, R18-P2's "headroom is fine" claim is
unverifiable post-cutover.

**Optimisation**:
1. Confirm `restore_handler` emits `attempts_used` on Ok path (a
   `tracing::info!(attempts_used=N, "vm_index_reserved")` is
   sufficient; expensive only if not already there).
2. Histogram for p50/p99 attempts used would let us right-size the
   budget — if p99 is 5 attempts at fence=30, the 36-attempt ceiling
   is fine and we can think about *lowering* fence (faster CREATE
   path) without risking wakes.

**Win**: instrumentation, not SLO; enables Tier-4 work (fence
right-sizing). **Risk**: ~zero. ~5 LOC.

## MINOR

### R18-M1 — `ON CONFLICT DO NOTHING` perf cost is negligible

The partial UNIQUE INDEX `wake_jobs_sandbox_pending_uniq` adds:
- **One btree-node maintenance** per INSERT (insert if winner / no-op
  if loser). On a small index (active wakes ≈ concurrently-waking
  sandboxes; c=20 stress → ~20 entries), the index fits in a single
  postgres page. Cost: sub-µs server-side, dwarfed by the 1-3 ms RTT.
- **No constraint-check roundtrip**: `ON CONFLICT` is server-side,
  same statement. We do NOT pay a "check then insert" 2-RT round.

INFO.

### R18-M2 — GC sweep per-row cost is negligible

`gc_expired_wake_jobs` (`db.rs:3194`) is a single indexed DELETE
filtered on `state IN ('ok','failed') AND updated_at < now() - $1`.
At 60-s cadence × 5-cycle window before terminal row deletion =
~5 deletes/row maximum (effectively 1 — the row is dropped on the
first sweep after `updated_at + T_KEEP`). At c=20 stress with
~200 wakes/s steady state, retention 300 s → ~60K rows in flight.
The sweep deletes ~12K rows/min in steady state (200 wakes/s × 60 s).
Single DELETE … WHERE indexed scan: ~50-200 ms server-side, ~1 ms
RTT. Bounded by `idx_wake_jobs_state` (presumed; verify) — but the
DELETE statement doesn't bound `LIMIT`, so a backlog accumulates
linearly until the next tick. At p99 stress this is fine; at
sustained 1000 wakes/s it would spike DELETE wall-time.

**Optimisation (low priority)**: add `LIMIT 5000` to the DELETE
(pg supports `DELETE … WHERE ctid IN (SELECT ctid … LIMIT N)`) so
each sweep is bounded; rely on cadence to drain. Saves nothing at
c=20; future-proofs for 10× scale.

INFO at current load. **Risk**: ~zero. ~5 LOC.

## Quantified-win roadmap (updated)

| Finding | File:Line | Win | Effort | Risk |
|---|---|---|---|---|
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT 1.5 s + WAKE 2 s + 95% storage | ~200 LOC + format-rev | medium |
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | ~1.0-2.0 s/SNAPSHOT | ~80 LOC + tests | medium |
| **R16-P2** livez timeout 500 → 200 ms | `nomad_ch.rs:3091`, `restore_handler.rs:2427` | ~900 ms WAKE, ~600 ms CREATE | 2 LOC | low |
| **R11-P1** Per-thread pg pool (raised priority) | `db.rs:515-521` | ~400-600 ms/wake controller CPU + SLO floor | ~150 LOC (per-worker thread_local) | medium |
| **R17-P1** Skip intermediate set_state writes / batch terminal | `wake_machine.rs:358, 381, 443-456, 128` | ~4-10 ms/wake | ~30 LOC + transaction | medium |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC (DashMap on AppState) | low |
| **R18-P1** Drop pre-INSERT idempotency SELECT | `admin_handlers.rs:1565-1587` | ~5-15 ms × 100% wake POSTs (~1-3 s pg/s @ c=20) | ~25 LOC removed | low |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:185`, `admin_handlers.rs:1585-1604` | ~2-6 ms/wake | ~15 LOC | ~zero |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms (CREATE + WAKE) | 2 LOC | ~zero |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low |
| **R18-P2** vm_index attempts-used metric | `restore_handler.rs:288-348` | instrumentation; enables fence right-sizing | ~5 LOC | ~zero |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s off L2 background (moot post-R16-P5) | 1 LOC | ~zero |
| **R17-P3** Static SQL constants | `db.rs:3030-3039` | trivial; prep-cache friendliness | ~8 LOC | ~zero |
| **R17-P4** Drop `.to_string()` cruft | `db.rs:2969, 3000, 3043` | sub-ms allocator | ~5 LOC | ~zero |
| **R18-M2** Bounded GC sweep DELETE LIMIT | `db.rs:3194-3211` | future-proof @ 10× scale | ~5 LOC | ~zero |
| **R16-P8** L1 hit/miss metrics | `snapshot_store_gcs.rs:1173-1201` | enables Tier-4 pre-warm | ~10 LOC | ~zero |

**Top-3 SLO-visible wins (unchanged from r17):**
1. **R16-P5 gzip** — ~3.5 s SNAPSHOT+WAKE + 95% storage. (Largest.)
2. **R16-P3 fused AEAD+SHA** — ~1.5 s/SNAPSHOT RPC critical path.
3. **R11-P1 per-thread pg pool** — ~400-600 ms controller CPU on the
   polling endpoint at c=20.

## Cross-lens consensus

- **Architecture / concurrency r17**: GATE-C2 closes R17-C2. Perf
  lens agrees: the partial UNIQUE INDEX is the right surface — moves
  correctness *into* the database where it belongs, costs ~0 ms on
  the happy path, and the +1 pg roundtrip on the race-loser path is
  strictly better than the previous 500 (the race-loser was
  previously returning a corrupted state, not a fast 200).
- **API surface**: `replay: true` envelope unchanged in shape between
  the pre-INSERT precheck and the post-INSERT race-loser branch —
  consumers can't distinguish them. R18-P1 (dropping the precheck) is
  a perf-only refactor with no wire impact.
- **Code quality**: R18-P1 also simplifies the handler (one branch
  point instead of two for the same outcome). Pair with R17-P1c.
- **Security**: no change — `wake_id` is still typed-id, `lessee` is
  still controller host, sanitisation pinned by R16-S2.

## Lens hand-off

**Tier 2 (1 PR, ~1.0 s SLO win, low risk):** R16-P1 + R16-P2
(cadence + timeout) — unchanged from r17.

**Tier 2.5 (1 PR, ~10 ms/wake + ~400 ms/wake controller CPU + ~5-15
ms/wake POST, low risk):** R17-P1c + R17-P3 + R17-P4 + R17-P2 +
**R18-P1** + **R18-P2 metric**. Bundle: all touch the PR2/GATE-C2
surface with no shared logic with Tier-3. R18-P1 specifically rides
on GATE-C2's correctness guarantee (precheck no longer load-bearing
for the race).

**Tier 2.75 (1 PR, R11-P1; ~400-600 ms controller CPU + SLO floor
restoration, medium risk):** unchanged. More urgent post-PR2 + GATE-C2
— the wake-POST and poll paths both pay the handshake tax.

**Tier 3 (2 PR, ~5 s SLO win, medium risk):**
- R16-P3 fused encrypt + SHA (PR-a).
- R16-P5 gzip-before-encrypt (PR-b, depends on PR-a's chunk-loop).

**Tier 4 (1 PR, instrumentation):** R16-P8 L1 hit/miss metrics +
R18-P2 attempts-used metric + R18-M2 bounded GC LIMIT. Blocks
pre-warm + fence right-sizing.

R16-P4 + R16-P7 + R17-P1a/b (intermediate-write skips, terminal
batch) defer until Tier-3 lands — they're polish, not SLO-floor
moves.

## Carry-forward (r17)

All r17 items remain OPEN. **R11-P1 priority still raised** post-PR2.
R15-P1, R14-P3 stay INFO. **Closed since r17: none.** R17-C2
correctness landed; perf-side delta tracked above as R18-P1 + R18-M1.

## Ranked next-biggest perf lever (updated)

1. **R16-P5 gzip** — ~3.5 s SLO + 95% storage.
2. **R16-P3 fused encrypt + SHA** — ~1.5 s/SNAPSHOT.
3. **R16-P2 livez timeout** — ~900 ms/WAKE.
4. **R11-P1 per-thread pg pool** — ~400-600 ms controller CPU.
5. **R17-P2 active-set wake cache** — ~300-500 ms wasted pg/wake polling.
6. **R18-P1 drop pre-INSERT idempotency SELECT** — ~5-15 ms × 100%
   wake POSTs (NEW post-GATE-C2).
7. **R9-P1 AEAD wake hard-link** — ~0.5-1.5 s/AEAD wake.
8. **R17-P1 wake-machine pg-roundtrip trim** — ~4-10 ms/wake.
9. **R16-P1 alloc cadence** — ~150 ms CREATE+WAKE.
10. **R10-P6 cached ureq::Agent** — ~30-100 ms/wake.

## Focus-area notes (brief)

**(1) GATE-C2 INSERT perf**: Partial UNIQUE INDEX adds ~0 ms on
happy path (btree-node maintenance is sub-µs vs 1-3 ms RTT). Race-
loser SELECT (`find_pending_wake_for_sandbox` re-read in `db.rs:3049`)
costs ~5-15 ms but fires only on actual contention. R18-P1: drop the
pre-INSERT precheck — GATE-C2 means it's no longer load-bearing, and
removing it eliminates a SELECT from EVERY wake POST.

**(2) C-7-LT-1 retry-loop cost**: 70 s ceiling at fence=30 (vs 50 s
sync). +20 s worst case ONLY when source teardown is slow; p50 wake
unaffected (loop exits on first successful `reserve_vm_index`).
Headroom-usage metric (R18-P2) recommended pre-cutover so we can
verify p99 isn't living near the 36-attempt ceiling.

**(3) GC sweep cadence × T_KEEP**: 60 s cadence × 300 s T_KEEP =
~5 sweep windows before a terminal row is deleted. Per-sweep cost:
single indexed DELETE; ~ms at c=20. Future-proof: add LIMIT (R18-M2)
so 10× scale doesn't spike DELETE wall-time.

**(4) Roadmap re-check**: Tier-3 unchanged. R18-P1 promoted to Tier
2.5 (rides on GATE-C2 correctness). R18-P2 + R18-M2 join Tier-4
instrumentation/future-proof.

**(5) Post-cutover SLO projection**: unchanged from r17 (p50 ~5.6 s,
p99 ~8.5 s pre-Tier-3; ~4.7 s / ~6.5 s with R16-P2 + R11-P1 + R16-P5
applied).
