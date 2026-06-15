# Metering / billing hot-path load measurement (#29)

**Date:** 2026-06-14
**Branch:** `feat/billing-metering`
**Harness:** `crates/control/benches/metering_load.rs` (+ `tests/metering_rowlock_pgbench.sh`)
**DB:** dedicated `zeroship_metering_load` on PostgreSQL 17.7 :5440 (full Liquibase changelog, 165 changesets)
**Env:** shared dev box; PG `max_connections = 100` (3 reserved, ~6 used by the co-resident stack)

This is a **measurement** report. Every number below is measured on the real
ingest / spend / reconcile code paths against real Postgres. No estimates. Where
the environment imposed a ceiling, that ceiling is reported as a finding rather
than papered over.

---

## TL;DR

1. **The deferred `usage_aggregates` hot-row UPSERT contention is REAL and severe
   under contention-isolated load.** With persistent pooled connections, a single
   `(app_id, period, metric)` row caps at **~400–470 UPSERTs/sec no matter how
   many writers push to it** — flat throughput, latency growing linearly
   (2 ms → 119 ms as clients go 1 → 48). Spreading the same write rate across
   10 000 rows scales linearly to **~10 000 tps at a flat ~4.8 ms** — a **~17–25×
   gap at 32–48 concurrent writers.** This is the textbook hot-row serialization
   the design flagged.
2. **At the concurrency the dev box can actually sustain through the REAL
   `Metering::ingest_at` path, the hot-row tax is MASKED by a different, more
   immediate bottleneck: the no-pool `Registry` opens a fresh SCRAM-authenticated
   PG connection PER report.** That ~14 ms/report connection floor dominates, and
   `max_connections=100` caps useful ingest concurrency at ~32 (resets-by-peer
   above that). **The connection model, not the row lock, is the first wall the
   real path hits today.**
3. **Spend fleet sweep** (`SpendEngine::evaluate_all`) is **O(N) serial per app on
   one connection**, ~**3.1–4.0 ms/app** → ~3.4 s at 1 000 apps, ~16–20 s at
   5 000 apps. The #5 connection-storm fix (one conn for the sweep, batched usage
   prefilter) holds; the remaining cost is the per-app state read + freshness
   write loop.
4. **Reconcile read pattern is confirmed O(active), NOT O(all-apps-ever).** With
   the fleet fixed at 4 000 owned apps, the per-app read time scales with the
   ACTIVE count (100 → 1 000 → 4 000) at a flat **~0.73 ms/active-app**, while the
   owner-grouping + active-prefilter stays ~5–9 ms regardless. The earlier
   bulk-prefilter perf fix is intact.

**Recommendation:** the deferred `usage_aggregates` sharding (slot = worker_id,
or per-worker append-only + SUM-on-read) **is warranted as a pre-scale item, but
is NOT the first bottleneck to fix.** The connection model (a pooled control-side
PG path, or batching multiple reports per tx/connection on ingest) gates ingest
throughput *before* the row lock does on the real path. Once that mask is removed
(pooling), the hot-row lock becomes the hard ceiling — and the pgbench numbers
below quantify exactly how hard. See **§5 Recommendation** for the data-backed
call and sequencing.

---

## 1. How to reproduce

```bash
# One-time: dedicated DB + full changelog.
createdb -h localhost -p 5440 -U postgres zeroship_metering_load
docker run --rm --network host -v "$PWD/db/changelog:/liquibase/changelog:ro" \
  liquibase/liquibase:4.31 \
  --url=jdbc:postgresql://localhost:5440/zeroship_metering_load \
  --username=postgres --password=zeroship \
  --changelog-file=changelog/db.changelog-master.yaml --liquibase-schema-name=public update

# Rust harness (real ingest / spend / reconcile paths):
METERING_LOAD_DB='postgres://postgres:zeroship@localhost:5440/zeroship_metering_load' \
  cargo bench -p zeroship-control --bench metering_load -- all     # or: ingest | spend | reconcile

# DB-level row-lock isolation (persistent pooled connections):
PSQL=$(command -v psql) PGBENCH=$(command -v pgbench) \
METERING_LOAD_DB='postgres://postgres:zeroship@localhost:5440/zeroship_metering_load' \
  tests/metering_rowlock_pgbench.sh
```

The Rust harness is gated on `METERING_LOAD_DB` (absent ⇒ prints a how-to note
and exits 0, so a plain `cargo bench` in CI stays green). It is self-cleaning:
it `TRUNCATE`s its own working tables on entry (guarded to refuse any DB whose
name is not `zeroship_metering_load`).

---

## 2. Bench 1 — ingest throughput + hot-row contention (the headline)

### 2a. Through the REAL `Metering::ingest_at` path (no-pool, connection-per-report)

Each task calls the real per-report transaction: the `(worker_id, sequence)`
dedup `INSERT … ON CONFLICT DO NOTHING` + the `(app_id, period, metric)`
`usage_aggregates` UPSERT. HOT = few apps (heavy lock contention on a handful of
PK rows); SPREAD = one app per task (contention dispersed). Representative run
(numbers vary run-to-run on the shared box; the *pattern* is stable):

| scenario  | apps | conc | reports/s | p50 ms | p95 ms | p99 ms |
|-----------|-----:|-----:|----------:|-------:|-------:|-------:|
| hot-1app  |    1 |    1 |        68 |  14.0  |  17.3  |  22.6  |
| hot-1app  |    1 |    8 |       225 |  32.7  |  54.2  |  69.4  |
| hot-4app  |    4 |    8 |       219 |  34.7  |  50.1  |  64.9  |
| spread    |    8 |    8 |       223 |  32.9  |  54.9  |  70.2  |
| hot-1app  |    1 |   16 |       216 |  56.8  | 167.0  | 275.5  |
| hot-4app  |    4 |   16 |       221 |  57.1  | 159.5  | 214.5  |
| spread    |   16 |   16 |       217 |  59.9  | 158.1  | 236.0  |
| hot-1app  |    1 |   32 |       213 |  103.6 | 398.6  | 573.2  |
| hot-4app  |    4 |   32 |       209 |  107.6 | 393.4  | 593.9  |
| spread    |   32 |   32 |       218 |  102.7 | 386.7  | 548.0  |

**What this shows:** throughput plateaus at ~210–280 reports/s and is *roughly
the same for HOT and SPREAD*. The hot-row tax is visible (in the lower-noise runs
hot-1app's p99 ran ~1.5–1.7× the spread/4-app p99 at conc 16, and one run hit
hot-1app p99 = 1 642 ms at conc 32), but it is **not the dominant cost here** —
because the no-pool `Registry` opens a fresh PG connection *per report*. That
per-report connection-open + commit (~14 ms floor at conc 1) is what caps
throughput, and it costs the same whether or not the row is contended. Above
conc ≈ 32 the connection churn (detached driver teardown lag stacking dead
backends toward `max_connections=100`) produces "Connection reset by peer" — the
harness counts these as `conn-ceiling errs` rather than crashing.

> **Finding A (connection model).** The control-plane `Registry` has no pool; it
> opens one connection per query. On the metering ingest path that is one
> connection *per report*. This makes (a) the per-report connection-establish
> cost the throughput floor, and (b) `max_connections` the concurrency ceiling.
> This masks the row-lock cost on the real path today.

### 2b. Contention-isolated (pooled connections, `tests/metering_rowlock_pgbench.sh`)

To measure the *pure* row-lock contention, pgbench holds one persistent
connection per client and runs the bare `usage_aggregates`-shaped UPSERT — HOT
(every client → ONE row) vs SPREAD (each tx → a random row of 10 000). 5 s/cell.

| clients | HOT tps | HOT lat avg | SPREAD tps | SPREAD lat avg |
|--------:|--------:|------------:|-----------:|---------------:|
|       1 |     428 |      2.3 ms |        415 |         2.4 ms |
|       4 |     476 |      8.4 ms |        934 |         4.3 ms |
|       8 |     450 |     17.8 ms |      1 863 |         4.3 ms |
|      16 |     459 |     34.9 ms |      3 721 |         4.3 ms |
|      32 |     429 |     74.6 ms |      7 184 |         4.5 ms |
|      48 |     433 |    110.9 ms |     10 699 |         4.5 ms |

(Reproduced across multiple runs; e.g. a detailed 32-client cell: HOT processed
1 949 tx in 5 s = 386 tps @ 82.8 ms avg; SPREAD processed 33 602 tx = 6 763 tps
@ 4.7 ms avg.)

> **Finding B (the headline — hot-row contention).** A single `(app_id, period,
> metric)` row serializes ALL concurrent UPSERTs on its row lock. HOT throughput
> is **flat at ~400–470 tps** regardless of writer count; latency grows linearly
> with concurrency (2 → 111 ms). SPREAD scales linearly to **~10 700 tps** with
> flat ~4.5 ms latency. **The contention curve plateaus immediately: one hot row
> = ~450 UPSERTs/sec hard cap, ~17–25× below the dispersed case at 32–48 writers.**

**Where it plateaus:** the HOT row plateaus at the *first* concurrency step —
there is essentially no throughput scaling at all (428 → 476 → 450 → 459 → 429 →
433 tps from 1 → 48 clients). All added concurrency converts directly to queueing
latency. This is the exact failure mode the design pre-staged.

---

## 3. Bench 2 — spend fleet sweep (`SpendEngine::evaluate_all`)

Seeds N apps each with one current-period `usage_aggregates` row, then times the
real `evaluate_all` (bulk plan/weights/FX hoist + batched fleet-usage prefilter +
per-app derive + per-app `app_spend_state` freshness write).

| apps  | sweep ms | apps/sec | ms/app |
|------:|---------:|---------:|-------:|
|   100 |     586  |     171  |  5.86  |
| 1 000 |   3 436  |     291  |  3.44  |
| 5 000 |  15 923  |     314  |  3.18  |

> **Finding C (spend sweep).** The #5 connection-storm fix holds — the sweep runs
> on ONE connection and prefilters fleet usage in a single query (it does NOT open
> ~2N connections). The residual cost is **O(N) sequential per-app**: `app_state_row`
> read + `derive_state` + `touch_state`/`persist_transition` write, ~3.1–4.0 ms/app.
> At 5 000 apps the sweep is ~16 s; with the cron's default ~60 s tick that leaves
> headroom to ~15–18 k apps before a sweep risks overrunning its interval. Beyond
> that, the per-app loop (not connections) is the scaling limit — a future
> batched/pipelined derive would help, but it is not urgent at current scale.

(Note: `evaluate_all` reads `apps` fleet-wide, so it also prices any apps left by
other benches; "+0 transitions" above reflects the seeded usage staying under the
default cap — the per-app read+write work still executed for every app, which is
what the timing measures.)

---

## 4. Bench 3 — reconcile read pattern: O(active) vs O(all-apps-ever)

Replays the real `billing_reconcile::sweep` READ shape over a CLOSED period: the
owner-grouping query + the active-app `usage_aggregates` prefilter, then per-ACTIVE
app `Metering::period_totals_on` + the plan lookup. (Stripe POSTs + invoice writes
are out of scope here — they are a fixed per-active-CREATOR network cost, not part
of the "scales with all-apps-ever" question.) Fleet fixed at **4 000 owned apps**;
ACTIVE fraction varied.

| total apps | active | owner-group ms | per-app reads ms | ms/active-app |
|-----------:|-------:|---------------:|-----------------:|--------------:|
|      4 000 |    100 |          5.1   |          73.0    |     0.73      |
|      4 000 |  1 000 |          6.4   |         725.2    |     0.73      |
|      4 000 |  4 000 |          9.3   |        2936.3    |     0.73      |

> **Finding D (reconcile is O(active)).** With total apps FIXED at 4 000, the
> per-app read time tracks the ACTIVE count, not the total: ms/active-app is flat
> at **~0.73 ms**. If reconcile were O(all-apps-ever) all three rows would show the
> same ~2 900 ms (4 000 apps each). They do not — the bulk active-app prefilter
> (the earlier perf fix) correctly drops inactive apps before the per-app loop.
> The owner-grouping + prefilter is cheap and near-constant (~5–9 ms).

---

## 5. Recommendation — is the deferred `usage_aggregates` sharding warranted?

**Yes — warranted as a pre-scale item, but correctly SEQUENCED behind the
connection model.** The data:

- **The hot-row lock is a hard ~450 UPSERT/sec/row ceiling (Finding B).** It does
  not scale with writers at all — it is a wall, not a slope. For a popular app
  whose traffic is metered by many workers all flushing into the SAME
  `(app_id, current_period, requests)` row, this is exactly the contention the
  design flagged. The 17–25× HOT-vs-SPREAD gap is the measured justification.
- **But on the REAL ingest path TODAY, the no-pool connection model (Finding A)
  is the FIRST wall** — it caps a single control instance at ~210–280 reports/s
  and ~32 concurrent flushers before the row lock even becomes the binding
  constraint. Sharding the aggregate row without addressing connections would
  move the bottleneck only marginally.

**Sequencing the data supports:**

1. **First: connection model on the ingest path.** Pool the control-side PG
   connections (or batch multiple reports per transaction/connection so ingest is
   not one-connection-per-report). This raises the real-path ceiling toward the
   pooled numbers in §2b and is the prerequisite for the row lock to even matter.
2. **Then: shard the hot `usage_aggregates` row.** Once pooling removes the
   connection mask, a single hot row caps the whole platform's metering of one
   popular app at ~450 UPSERTs/sec. Of the two staged designs:
   - **slot = worker_id sharding** (`PK (app_id, period, metric, slot)` where
     `slot` is a small hash of `worker_id`): bounded fan-out (== worker count),
     contention dispersed across `slot` rows, `SUM(total)` on read. Lowest read
     amplification; recommended.
   - **per-worker append-only + SUM-on-read**: simplest writers (pure INSERT, zero
     UPSERT contention), but unbounded row growth per period → heavier reads + a GC
     burden. Prefer the bounded slot design unless write-amplification on the
     UPSERT proves worse than the slot fan-out read cost.

   The slot count is the knob: it trades read-side `SUM` cost against write-side
   contention. The §2b curve says even **8 slots** would lift a hot app from ~450
   to ~3 700 UPSERTs/sec (the SPREAD-over-8-ish regime), and **32–48 slots**
   recovers near-linear scaling.

3. **Spend sweep + reconcile do NOT need work now (Findings C, D).** Reconcile is
   already O(active); the spend sweep is O(active-fleet) on one connection with
   ~15–18 k-app headroom under the default tick. Revisit the spend sweep's per-app
   serial loop only past ~10 k active apps.

**Net:** the deferred sharding is justified by data (Finding B), but it is the
*second* lever, not the first. The honest pre-launch posture: keep the sharding
in the staged-design backlog, implement connection pooling on the metering ingest
path first, and land the `slot = worker_id` aggregate sharding before any single
app is expected to sustain >~450 metered events/sec from concurrent workers.

---

## 6. Environment ceilings hit (honest caveats)

- **`max_connections = 100`** on the shared dev PG (3 reserved, ~6 used by the
  co-resident stack) capped the REAL-path ingest concurrency at ~32. Above that
  the no-pool churn produced resets-by-peer. The §2b pgbench measurement (pooled,
  ≤48 clients) is the unconstrained view of the row lock and is the authoritative
  contention curve; the §2a real-path numbers are the constrained-but-faithful
  view of what one control instance does today.
- **Shared box variance.** Run-to-run ingest latency tails varied (occasional
  multi-hundred-ms / multi-second outliers in *both* HOT and SPREAD), attributable
  to connection-establishment stalls + co-tenant load, not row-lock waits — which
  is itself consistent with Finding A. The pgbench contention delta (§2b) is stable
  across runs; the spend (§3) and reconcile (§4) timings are stable to within a few
  percent.
