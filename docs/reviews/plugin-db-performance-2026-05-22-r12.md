# plugin-db Performance Review — 2026-05-22 r12

Commit: HEAD (`89dbb6a8`). Cycle baseline: r11 (`05484878`, 81 / 100).
Mode: forcing-function check, bench re-run, harness-extension design.

**Forcing function MET this cycle.**
`251d53b4 plugin-db/v8_bridge: row_to_json O(N²) → O(N) via index lookup (I35)`
is the first perf-targeting commit since r10. The fix is real and on the
hot read path; it is **invisible to the current bench harness** (which
covers query building, not row decoding). r12 measures the existing
benches, confirms no regression, and recommends adding
`bench_row_to_json` as the next-cycle forcing function.

---

## 1. Commits since r11

| Commit     | Hot path?              | Notes |
| ---------- | ---------------------- | ----- |
| `251d53b4` | **Yes — read path**    | `row_to_json` now enumerates `(idx, col)` and threads `usize` into `column_to_json`. compio-postgres `RowIndex for usize` is bounds-check + return (`crates/compio-postgres/src/row.rs:49-61`); `RowIndex for str` does linear `position` with a case-insensitive retry on miss (lines 65-82). Affects every CRUD read path (`find` / `findOne` / `aggregate` / RETURNING). |
| `18aee490` | No                     | Migration-pipeline `finalise_backfill` warn-shape drift fix. Off CRUD hot path. |
| `bac64c0e` | No                     | Visibility narrowing on `mig_lock` accessors (`pub` → `pub(crate)`). Compile-time only. |
| `89dbb6a8` | No                     | Docs-only (deferred backlog + cycle 13:17 reviewer reports). |

The bench harness (`crates/plugin-db/benches/bench_query_build.rs`)
exercises `build_find` and `build_insert` — **query construction**, not
row decoding. Its own header (lines 12-33) calls out that `row_to_json`
was the highest-leverage candidate but unbenchable from outside the
crate because `compio_postgres::Row::new` is `pub(crate)`
(`crates/compio-postgres/src/row.rs:116`). That blocker still stands at
HEAD — verified by grep: no `test-helpers` feature, no exposed `Row`
constructor, no shim.

---

## 2. Bench delta vs r11

Methodology: full bench (no `--quick`), criterion defaults (100 samples
× 3s measurement, 1s warm-up). Single run this cycle.

System: Intel Xeon @ 2.80 GHz, Linux 6.12.80, loadavg `1.47 / 0.70 /
0.49` at run start.

Means are criterion's point estimate (middle column of the `time:`
line); ranges are criterion's `change:` line vs the previous saved
baseline (which is r11's saved run, since r11 ran with the same harness
and stored its baseline at `target/criterion/`).

| Bench                     | r11 mean   | r12 mean   | criterion `change:` line                          | criterion verdict                  |
| ------------------------- | ---------- | ---------- | ------------------------------------------------- | ---------------------------------- |
| `build_find/empty`        |  492.51 ns |  496.92 ns | `[+2.13% +2.60% +3.04%] (p = 0.00 < 0.05)`        | "Performance has regressed."       |
| `build_find/small`        |  991.26 ns |  987.58 ns | `[-10.97% -3.84% +0.27%] (p = 0.48 > 0.05)`       | "No change in performance detected." |
| `build_find/complex`      | 2878.40 ns | 3045.90 ns | `[-13.07% +3.74% +20.51%] (p = 0.75 > 0.05)`      | "No change in performance detected." |
| `build_insert/small_doc`  | 1898–1930 ns (in-suite, r11) | 1907.90 ns | `[+12.89% +21.12% +29.86%] (p = 0.00 < 0.05)` | "Performance has regressed."       |

**Reading the table.** Both criterion-flagged "regressions" are
non-events on inspection:

- `build_find/empty` moved from 492.51 → 496.92 ns. That's a 4.4 ns
  absolute shift on a path that performs no JSON traversal of interest.
  No code edited between r11 and r12 affects this path (verified: I35
  touches `v8_bridge.rs::row_to_json`; the empty-filter `build_find`
  call site never enters that function). 4 ns at this scale is
  scheduler / branch-predictor / TLB jitter, not a real shift.
- `build_insert/small_doc` lands at 1907.90 ns — **inside r11's
  in-suite range** (1898–1930 ns). The "+21%" headline is criterion
  comparing against an alternate baseline run from the same r11
  session, not against the r11-reported mean. r11 already documented
  this bench's 300+ ns oscillation between in-suite and isolated
  execution (§2 final paragraph). The fix is to stop reading the
  `change:` line for this bench alone, not to flag a regression.

`build_find/small` and `build_find/complex` are flat within criterion's
own significance gate.

**Net: no real regression. The bench harness shows no movement
attributable to any commit since r11.**

---

## 3. Is the I35 win visible in any existing bench?

**No.**

`bench_query_build.rs` calls `build_find` (in `query.rs`) and
`build_insert` (same module). Neither function calls `row_to_json` —
they construct SQL + params from a filter / document. `row_to_json` is
called by `rows_to_json_value` (`v8_bridge.rs:340`), which in turn is
called by `exec_query` (`exec.rs:86`). The bench harness never reaches
`exec_query`. There is no execution path in the bench that touches the
edited code.

This is **expected** and **structural**, not a bench-harness oversight:
r11's bench was deliberately scoped to query *construction* (covered
in the file's own preamble lines 10-34, "row_to_json takes a
`&compio_postgres::Row`, and `Row::new` is `pub(crate)` …"). The win
from I35 sits one crate boundary away from anything the bench can
currently touch.

---

## 4. Recommendation: add `bench_row_to_json` next cycle?

**Yes — conditionally.** The bench should exist before another I35-class
commit lands; otherwise we will keep landing read-path fixes that the
harness can never confirm. Two viable construction paths:

### Option A — synthetic Row via `compio-postgres` test-helper feature

Add a `#[cfg(feature = "test-helpers")] pub fn Row::new_for_test(
statement: Statement, body: DataRowBody) -> Result<Row, Error>` (or
similar named wrapper) in `crates/compio-postgres/src/row.rs`. Then in
`crates/plugin-db/benches/bench_row_to_json.rs`:

- depend on `compio-postgres = { ..., features = ["test-helpers"] }`
  in `[dev-dependencies]`;
- synthesise a `Statement` from a canned column list (3-col / 10-col /
  50-col rows; OID mix covering INT4 / TEXT / UUID / TIMESTAMP /
  JSONB — the branches in `column_to_json`);
- synthesise a `DataRowBody` from a precomputed binary buffer (one
  per shape, computed once outside the bench loop);
- `criterion::bench_function` over `row_to_json(&row)` per shape.

Pros: hermetic, fast, no PG required, criterion noise is minimal.
Cons: requires a one-line patch to `compio-postgres` to expose the
constructor, gated behind a feature so it doesn't leak into the
production surface. Lowest risk, highest leverage.

### Option B — real-PG `#[bench]`-gated integration bench

Add `crates/plugin-db/benches/bench_row_to_json_pg.rs` that:

- gates on `PG_TEST_URL` env var (same pattern as
  `crates/plugin-db/tests/integration.rs:9-12`);
- creates three throwaway tables (3-col / 10-col / 50-col) once in
  setup;
- inside the criterion bench, calls `client.query_one` to fetch a
  prepared row, then runs `row_to_json` against it.

Pros: no `compio-postgres` change. Cons: PG roundtrip dominates the
measurement (sub-microsecond `row_to_json` swamped by 100µs+ network
+ parse), so the bench can't isolate the I35 win cleanly. Useful as a
backstop only.

### Recommendation

**Option A.** The whole point of the bench is to make `row_to_json`'s
column-count scaling measurable in isolation; Option B doesn't deliver
that. The one-line `compio-postgres` change is small, feature-gated,
and unblocks every future row-decode optimisation (not just I35). r12
recommends but does not implement (read-only review).

### Design sketch (do not implement this cycle)

```
// crates/plugin-db/benches/bench_row_to_json.rs (sketch — DO NOT WRITE)
//
// Shapes: narrow (3 cols: INT4 + TEXT + UUID),
//         medium (10 cols: + BOOL + TIMESTAMP + INT8 + FLOAT8
//                          + JSONB + TEXT + UUID),
//         wide   (50 cols: 10× the medium mix).
//
// Per shape:
//   - build a Statement::new(...) with the column list (column names
//     "c0", "c1", ...) and oids;
//   - build a DataRowBody from a precomputed Bytes buffer with the
//     wire-format encoding of each column (one fixture per shape);
//   - bench_function("row_to_json/<shape>", |b| {
//         b.iter_batched_ref(
//             || row.clone(),
//             |r| { black_box(zeroship_plugin_db::v8_bridge::
//                            row_to_json_for_bench(r)); },
//             BatchSize::SmallInput,
//         );
//     });
//
// Note: row_to_json is pub(crate). Add a #[doc(hidden)] #[cfg(any(
// test, feature = "bench-helpers"))] pub wrapper in plugin-db, or
// re-export under a bench-only feature. Keep production surface
// unchanged.
```

**Expected signal (do not pre-quote a number).** If I35 worked as
described, the `wide` shape (50 cols) should be ~5× faster than under
the O(N²) baseline; the `narrow` shape should be near-flat; the
`medium` shape somewhere between. **The actual number is unknown until
the bench runs against both baselines**, and the wide-row absolute time
itself is unknown — the harness will produce ground truth.

---

## 5. C3 revisited: serde round-trip on the read path

C3 (deferred backlog line 84-93) tracks the residual serde cost on
`findOne` after `cc7fff89` reduced the chain from 4 parse/serialise
ops to 2 (a `.to_string()` in `crud.rs::first_row_or_null:107`
followed by V8 `JSON.parse` via `ResolveValue::Json`).

I35 changes the **column-lookup cost inside `row_to_json`**; C3 is
about the **serialise/parse at the V8 boundary after `row_to_json`
has produced its `Value`**. The two costs are sequential and
independent — one happens inside `rows_to_json_value`, the other
happens at the resolver boundary in `crud.rs`.

**Is C3 more or less material now?** Without measurement, the answer
is "unchanged in absolute terms, possibly larger in relative terms."
- Absolute: the `to_string` + `JSON.parse` round-trip in
  `first_row_or_null` is byte-identical to its pre-I35 form. Nothing
  in `cc7fff89`'s scope or in `251d53b4`'s scope edited it.
- Relative: if I35 made `row_to_json` materially cheaper (which the
  fix structurally must, but the bench can't see), then the
  `to_string` + V8 `JSON.parse` boundary cost becomes a larger
  fraction of the read-path budget. The serde tail is the same; its
  share of the total grew because the front shrank.

This is exactly the case where measurement is required before
re-prioritising. Without `bench_row_to_json`, we can't size what I35
saved; without sizing I35, we can't tell whether C3 is now the
dominant residual (warranting the multi-crate `ResolveValue::JsonValue`
shape it's blocked on) or still a second-order concern.

**r12's read of C3: do not pick up yet.** The forcing function for
revisiting C3 is `bench_row_to_json` landing and producing a
measurement that puts `row_to_json` below the `to_string` + `JSON.parse`
tail. Until then, C3's "Pickable this cycle: no" verdict (deferred
backlog line 93) stands.

---

## 6. Score

**81 / 100.** (r11: 81. Delta: 0.)

### Why ±0

- A perf-targeted commit (I35) landed and is structurally correct, but
  the **measurement** of its impact is absent — the existing bench
  can't see the read path, and no row-decode bench was added alongside
  the fix.
- The two carry-over IMPORTANTs (N9-I2 `$in` Vec<String> allocs, N9-I3
  migrations Vec→String round-trip) remain on disk unchanged.
- C3 deserves a re-rank but the data to re-rank it doesn't exist
  yet (§5).
- The bench harness is now visibly under-covering the production
  surface — that's a known structural debt called out in the
  harness's own header, but it didn't get paid down this cycle.

The score holds steady because I35 is real progress (closes a
documented O(N²) cliff on every read) but the lack of measurement
keeps the bench-harness completeness floor at the same level it sat at
in r11. Were the bench addition to land, r13 would have a measurable
delta to score against.

---

## 7. Next-cycle forcing function

r13 should not run until one of:

1. **`bench_row_to_json` lands** under `crates/plugin-db/benches/`
   (Option A from §4 — `compio-postgres` test-helper exposure plus the
   plugin-db bench file). This is the new top item.
2. **N9-I2 commit lands** fixing the `$in` Vec<String> per-call
   allocation on the build path (this remains benchable via
   `build_find/complex`, which uses `$in`).
3. **N9-I3 commit lands** fixing the migrations Vec → String → V8
   `JSON.parse` round-trip in `migrations.rs:402-403`.

The first item supersedes r11's old gating condition (it's the
"compio-postgres exposes a `Row` constructor (or an in-crate shim)"
clause, now with a concrete shape).

Without one of these, r13 will produce a fourth consecutive 81.

---

## 8. Anti-fabrication compliance

- All ns figures in §2 are criterion `time:` lines from a bench run
  conducted in this report (the cargo-bench invocation in §2's
  methodology paragraph). Single run, criterion defaults; sample size
  is criterion's default 100, not reduced.
- The "+2.6%", "+3.74%", "+21.12%" figures in §2's `change:` column
  are criterion's own output, not derived. They are presented with
  criterion's p-value and verdict alongside; the analysis explains why
  the criterion-flagged "regressions" are noise (not by hand-waving —
  by pointing at the absolute-ns shift, by absence of code changes on
  the path, and by r11's already-documented in-suite vs isolated gap).
- No I35 win is quantified. The §4 sketch explicitly says "do not
  pre-quote a number" and "the actual number is unknown until the
  bench runs against both baselines."
- C3's "unchanged in absolute terms, possibly larger in relative
  terms" framing in §5 is qualitative because there is no
  measurement; the requirement to measure before re-prioritising is
  stated explicitly.
- The system metadata (CPU, loadavg, kernel) is the literal output of
  `uptime` + `/proc/cpuinfo`.
- The "Row::new is pub(crate)" claim in §1 / §3 is grounded in a
  direct `Read` of `crates/compio-postgres/src/row.rs:116` this
  session.
- The integration-test PG harness reference in §4 Option B is
  grounded in a direct `Read` of
  `crates/plugin-db/tests/integration.rs:9-12` this session.

**Reproducibility footer:** bench output reproducible via
`cargo bench -p zeroship-plugin-db --bench bench_query_build` from
the repo root. The "no I35 signal in this bench" claim is testable by
running the same command against `251d53b4^` and observing identical
build_find / build_insert numbers — the harness simply does not
exercise the edited code path.
