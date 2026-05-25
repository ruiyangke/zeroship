# Sandbox/snapshot-restore — performance r17 review

Date: 2026-05-25 (UTC)
HEAD at audit: `163724dc`. Lens previously reviewed at r16 (`792a7aa5`).
This round evaluates PR2 (WakeMachine state machine + `/wake` async/poll
+ wake_jobs GC sweep).

## Summary

**6 new findings**, none CRITICAL. PR2 trades wall-time predictability
for **9 extra pg roundtrips per wake** (~12-30 ms added latency on the
machine itself, not the wake SLO floor). The biggest perf concern is
that R11-P1 (per-thread pg pool) is now load-bearing for **6 bookkeeping
`update_wake_job_state` calls** that each open a fresh TCP+STARTUP+auth
handshake. Polling endpoint is well-shaped but uncached. R16's Tier-3
wins (P3 + P5) remain the next-biggest SLO levers.

Carry-forward from r16 unchanged: R16-P1/P2/P3/P4/P5/P6/P7/P8, plus
R15-P1, R14-P1/P3, R11-P1/P4, R10-P1/3/4/5/6/7, R9-P1/#6/#8.

## CRITICAL

None.

## IMPORTANT

### R17-P1 — Wake state machine: 9 extra pg roundtrips/wake vs sync path

**File:Line:** `crates/sandbox/src/wake_machine.rs:185, 224, 237, 242,
266, 358, 381, 413, 443, 455, 128`/`154`;
`crates/sandbox/src/admin_handlers.rs:1556, 1585, 1642`.

Counted pg roundtrips per wake:

| Path | Sandbox-row ops | Wake_job-row ops | Total |
|---|---|---|---|
| Sync (`do_restore_inner`) | 5 (`get_sandbox_row`, `read_snapshot_row`, 2× `update_sandbox_status`, `clear_snapshot_metadata`) | 0 | **5** |
| Async (handler + WakeMachine) | 6 (handler `get_sandbox_row` at 1585 + machine repeats at 185 = redundant; plus same 4 as sync) | 8 (`find_pending_wake` + `insert_wake_job` + 6× `update_wake_job_state` incl. terminal) | **14** |

Delta: **+9 pg roundtrips**, of which **6 are intermediate
`set_state(…)` writes** at `wake_machine.rs:237, 266, 358, 381, 413,
431`. At ~1-3 ms per roundtrip on local pg, that's **~6-18 ms added per
wake**; on a realistic ~2-5 ms RTT pg it's **~12-30 ms**. Acceptable
against a 5-10 s wake SLO floor, but the redundant `get_sandbox_row` at
machine line 185 (already done at handler line 1585 — the handler then
discards the row instead of passing it to the machine) is **free to
delete**: ~2-6 ms recovered.

**Optimisation options:**
1. **Skip `LivezPolling` and `ClockResyncing` intermediate writes** —
   both phases each take >1 s of real work, so the client polling at
   250 ms-1 s cadence will see `Restoring` for that whole window
   anyway. Two `update_wake_job_state` calls become zero; the
   `Registering` write at line 413 then serves as the marker that
   livez + clock-resync are done. Saves **~4-10 ms/wake**.
2. **Batch the final transitions** — the `update_sandbox_status →
   Running` (443), `clear_snapshot_metadata` (455), and terminal
   `update_wake_job_state(Ok)` (128) are 3 separate roundtrips for
   work that's already complete. A single CTE/multi-statement
   command (or short pg transaction) compresses to 1 roundtrip,
   saving **~4-10 ms/wake** off the very tail.
3. **Pass the pre-flight row from handler to machine** — eliminates
   the `get_sandbox_row` re-read at machine line 185.

**Risk**: medium. Option 1 changes the user-observable polling shape
(less granular). Option 2 is a tiny SQL refactor + 1 transaction.
Option 3 is a constructor field plumb.

### R17-P2 — Poll endpoint issues 1 pg query per poll, no cache

**File:Line:** `crates/sandbox/src/admin_handlers.rs:1741`
(`db.get_wake_job(&wake_raw)`), `crates/sandbox/src/db.rs:2986-3005`.

Every `GET /wake/{wake_id}` opens a fresh pg pool (handshake +
TCP), runs the query, returns the row. With recommended 250 ms-1 s
client polling cadence and 5-10 s wake durations, **5-40 polls per
wake**. At ~2-5 ms/query (single-row indexed lookup, but with
handshake amortised by the pool's idle-keep), each poll is dominated
by pool-open if R11-P1 still bites.

Two compounding issues:
1. **R11-P1 amplification**: each `get_wake_job` calls `open_pool()`
   (a fresh `Pool::connect_with_config`). That's a TCP + STARTUP +
   auth roundtrip per poll. ~5-15 ms per poll on the wire. **40 polls
   × 10 ms = 400 ms wasted/wake on connection setup alone**, not the
   actual SELECT.
2. **No active-set cache**: the in-flight `wake_jobs` rows for a
   controller are O(currently-waking sandboxes). At c=20 stress, that's
   ~20 rows — fits in a `DashMap<wake_id, WakeJobRow>` updated by the
   machine on each `set_state` and read by the poll handler. Poll
   becomes a sub-µs map lookup; only terminal/missing rows fall
   through to pg.

**Optimisation**: a process-local `Arc<DashMap<String, WakeJobRow>>` on
`AppState`. Machine writes to it in `set_state`; poll handler reads
it first, falls through to pg on miss. Eviction: drop on terminal +
`T_KEEP/2`. Saves **~300-500 ms of wasted pg work per active wake**
(off-SLO, but real controller CPU).

**Risk**: low. Cache is best-effort — pg is the source of truth.

### R17-P3 — `update_wake_job_state` re-formats SQL on every call

**File:Line:** `crates/sandbox/src/db.rs:3030-3039`.

```rust
let sql = format!(
    "UPDATE sandbox.wake_jobs … {ready_at_clause} WHERE wake_id = $5::TEXT"
);
```

Builds a new `String` per call. Cheap (~µs) but compounds at 6×/wake
× c=20 stress = 120 allocs/s. More importantly, the pg server can't
share the prepared-statement cache across the two SQL shapes (with vs
without `ready_at = now()`).

**Optimisation**: two static `&str` SQL constants, picked by a single
`if` at the call site. Server prepared-statement cache hits both
shapes after first run.

**Risk**: ~zero. 8 LOC refactor.

### R17-P4 — String allocs per `set_state` (5 per call)

**File:Line:** `crates/sandbox/src/db.rs:3043-3049`.

```rust
&state.as_str().to_string(),
&error_code.map(|c| c.as_str().to_string()),
&error_message.map(|s| s.to_string()),
&agent_url.map(|s| s.to_string()),
&wake_id.to_string(),
```

5 allocations per call × 6 calls/wake = 30 small Strings/wake. The
compio-postgres bind layer almost certainly accepts `&str` directly
— the `.to_string()` calls are defensive cruft from a previous
binding shape. Sub-ms total but inflates the allocator profile under
c=20 stress where 200+ wakes/s × 30 = 6000 small Strings/s on the GC
runtime.

**Risk**: ~zero. ~5 LOC. Same fix applies to `insert_wake_job` at
`db.rs:2969-2977` and `get_wake_job` at `db.rs:3000`.

## MINOR

- **R17-M1** `detach_isolated` per-wake cost ~5 ms (1 ms thread + ~3-5
  ms compio runtime). Amortised into 5-10 s wake — irrelevant. Don't
  call it inside a per-iteration loop. All current sites
  (`sweep.rs:248, 323, 658`, `admin_handlers.rs:1675`) spawn at coarse
  granularity. INFO.
- **R17-M2** Wake-jobs GC sweep query is a single indexed DELETE at
  60 s cadence. ~4 rows churn/min at c=20. ~zero.

## Quantified-win roadmap (updated)

| Finding | File:Line | Win | Effort | Risk |
|---|---|---|---|---|
| **R16-P5** gzip memory-ranges pre-AEAD | `snapshot_aead.rs:411`, `snapshot_store_gcs.rs:354` | SNAPSHOT 1.5 s + WAKE 2 s + 95% storage | ~200 LOC + format-rev | medium |
| **R16-P3** Fuse encrypt + canonical-SHA | `snapshot_aead.rs:413`, `snapshot_store.rs:184` | ~1.0-2.0 s/SNAPSHOT | ~80 LOC + tests | medium |
| **R16-P2** livez timeout 500 → 200 ms | `nomad_ch.rs:3091`, `restore_handler.rs:2427` | ~900 ms WAKE, ~600 ms CREATE | 2 LOC | low |
| **R11-P1** Per-thread pg pool (raised priority) | `db.rs:515-521` | ~5-15 ms/poll × 40 polls = **~400 ms/wake controller CPU**, plus restores SLO floor | ~150 LOC (per-worker thread_local) | medium |
| **R17-P1** Skip intermediate set_state writes / batch terminal | `wake_machine.rs:358, 381, 443-456, 128` | ~4-10 ms/wake | ~30 LOC + transaction | medium |
| **R17-P2** Active-set cache for poll | `admin_handlers.rs:1741` | ~300-500 ms wasted pg work/wake | ~60 LOC (DashMap on AppState) | low |
| **R17-P1c** Plumb pre-flight row into machine | `wake_machine.rs:185`, `admin_handlers.rs:1585-1604` | ~2-6 ms/wake | ~15 LOC | ~zero |
| **R16-P1** alloc_running 250 → 100 ms | `nomad_ch.rs:2668`, `restore_handler.rs:2399` | ~150 ms (CREATE + WAKE) | 2 LOC | ~zero |
| **R16-P4** L2 fuse 3 reads → 1 | `snapshot_store_gcs.rs:565-648` | 2 × 1 GB off background | ~120 LOC | low |
| **R16-P6** GCS chunk 8 → 32 MiB | `snapshot_store_gcs.rs:69` | ~3.8 s off L2 background (moot post-R16-P5) | 1 LOC | ~zero |
| **R17-P3** Static SQL constants | `db.rs:3030-3039` | trivial; prep-cache friendliness | ~8 LOC | ~zero |
| **R17-P4** Drop `.to_string()` cruft | `db.rs:2969, 3000, 3043` | sub-ms allocator | ~5 LOC | ~zero |
| **R16-P8** L1 hit/miss metrics | `snapshot_store_gcs.rs:1173-1201` | enables Tier-4 pre-warm | ~10 LOC | ~zero |

**Top-3 SLO-visible wins (unchanged from r16, +1 raised):**
1. **R16-P5 gzip** — ~3.5 s SNAPSHOT+WAKE + 95% storage. (Largest.)
2. **R16-P3 fused AEAD+SHA** — ~1.5 s/SNAPSHOT RPC critical path.
3. **R11-P1 per-thread pg pool** — raised priority. Was ~40 ms/wake in
   sync path; with PR2's 14 pg roundtrips on the wake path + 40 polls,
   it's now ~400-600 ms of avoidable handshake on the controller for
   every active wake. (Doesn't help SLO floor; helps c=20 stress
   capacity and controller CPU headroom.)

## Post-cutover SLO projection

With C-7-LT-PR1+PR2 fully landed (current HEAD `163724dc`) and no
Tier-3 wins yet, projected WAKE wall-time (no deadline cap):

```
submit_restore_job   ~2.5 s   (Nomad alloc + wrapper)
wait_for_livez       ~2.0 s   (CH guest agent boot)
store.get (L1-hit)   ~1.0 s   (AEAD-disabled) / ~1.5-2.0 s (active)
clock_resync         ~0.1 s
register_restored    <0.01 s
wake_machine pg      ~0.02 s  (14 roundtrips × ~1.5 ms; R17-P1 saves)
─────────────────────────────
p50 (L1-hit)         ~5.6 s
p99 (L1-miss, AEAD)  ~8.5 s
```

**Where is the next 100 ms - 1 s hiding?**
1. **R16-P2 livez timeout 500 → 200 ms** — first ~3 polls of agent
   boot waste ~900 ms; clean win, low risk. (1.0 s)
2. **R11-P1 per-thread pg pool** — 6 pg roundtrips × ~5 ms handshake
   each = ~30-90 ms inside the wake machine; another ~400 ms on the
   polling side. (0.5 s controller CPU.)
3. **R16-P5 gzip** — moves L1-miss WAKE from p99 ~8.5 s to p99 ~6.5 s.
   (2 s.)

With all three: **p50 ~4.7 s, p99 ~5.5 s, p99-cold ~6.5 s**.

Below 4 s p50 needs reducing `submit_restore_job` (Nomad floor, not
ours) or `wait_for_livez` (CH guest agent boot, not ours). Those are
the architectural floors we don't own.

## Cross-lens consensus

- **Architecture r17**: WakeMachine is the structural cutover the
  perf lens has been waiting for. Perf's bookkeeping cost (R17-P1) is
  the price of polling visibility; architecture agreed that giving
  it up trades observability for ~10 ms.
- **API surface**: R17-P2's cache is a perf-only change — wire shape
  unaffected (pg remains source-of-truth).
- **Code quality**: R17-P4 `.to_string()` cruft is also a quality
  signal; pair the fix when R11-P1's pool refactor lands.
- **Security**: poll endpoint at `admin_handlers.rs:1768` does NOT
  use constant-time comparison on sandbox_id mismatch. Documented
  in-code as acceptable; perf agrees — constant-time `eq` on a
  22-char base62 string is ~22 ns vs ~5 ns for `==`. Not a perf
  concern either way.

## Lens hand-off

**Tier 2 (1 PR, ~1.0 s SLO win, low risk):**
- R16-P1 + R16-P2 (cadence + timeout) — unchanged from r16.

**Tier 2.5 (1 PR, ~10 ms/wake + ~400 ms/wake controller CPU, low
risk):**
- R17-P1c (plumb row) + R17-P3 (static SQL) + R17-P4 (drop allocs) +
  R17-P2 (active-set cache). Bundle as one diff; all touch the same
  PR2 surface with no shared logic with Tier-3.

**Tier 2.75 (1 PR, R11-P1; ~400-600 ms controller CPU + SLO floor
restoration, medium risk):**
- R11-P1 per-thread pg pool. Now more urgent post-PR2 — handshake
  amplification on the polling endpoint is a real c=20 stress
  concern.

**Tier 3 (2 PR, ~5 s SLO win, medium risk):**
- R16-P3 fused encrypt + SHA (PR-a).
- R16-P5 gzip-before-encrypt (PR-b, depends on PR-a's chunk-loop).

**Tier 4 (1 PR, instrumentation):**
- R16-P8 L1 hit/miss metrics. Blocks pre-warm work.

R16-P4 + R16-P7 + R17-P1a/b (intermediate-write skips, terminal
batch) defer until Tier-3 lands — they're polish, not SLO-floor
moves.

## Carry-forward (r16)

All r16 items remain OPEN. **R11-P1 priority raised** post-PR2 (now
load-bearing on polling endpoint). R15-P1, R14-P3 stay INFO.
**Closed since r16: none.**

## Ranked next-biggest perf lever (updated)

1. **R16-P5 gzip** — ~3.5 s SLO + 95% storage.
2. **R16-P3 fused encrypt + SHA** — ~1.5 s/SNAPSHOT.
3. **R16-P2 livez timeout** — ~900 ms/WAKE.
4. **R11-P1 per-thread pg pool** — ~400-600 ms controller CPU post-PR2 (raised).
5. **R17-P2 active-set wake cache** — ~300-500 ms wasted pg/wake polling.
6. **R9-P1 AEAD wake hard-link** — ~0.5-1.5 s/AEAD wake.
7. **R17-P1 wake-machine pg-roundtrip trim** — ~4-10 ms/wake.
8. **R16-P1 alloc cadence** — ~150 ms CREATE+WAKE.
9. **R10-P6 cached ureq::Agent** — ~30-100 ms/wake.

## Focus-area notes (brief)

**(1) Wake-machine overhead**: 6 set_state + 1 terminal = ~12-30 ms on
realistic pg. Acceptable vs 5-10 s SLO. R17-P1c + R17-P3 + R17-P4
recover ~5-10 ms cleanly.

**(2) Polling cost**: 1 pg query × 5-40 polls/wake. Without R11-P1,
each query carries ~5-15 ms TCP+STARTUP+auth tax. R17-P2 (active-set
DashMap) is the surgical fix; R11-P1 is platform-wide.

**(3) Roadmap re-check**: R16-P5/P3/P2 unchanged. R11-P1 priority
raised. Post-Tier-3 layered: CREATE ~3.5 s, SNAPSHOT ~2-3 s,
WAKE ~4.7 s p50 / ~6.5 s p99-cold.

**(4) Next 100 ms - 1 s**: R16-P2 (livez ~900 ms), R11-P1 (~400 ms
poll-side handshake), R17-P2 (~300-500 ms wasted pg/wake). Then
floor is CH guest agent boot + Nomad alloc — not ours.

**(5) detach overhead**: ~5 ms/call; all current call sites are
coarse-grained (no per-iter loops). See R17-M1.
