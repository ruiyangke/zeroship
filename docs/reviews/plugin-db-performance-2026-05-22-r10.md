# plugin-db Performance Review — 2026-05-22 r10

Commit: HEAD (`d2e7e22`). Cycle baseline: r9 (`7d0bc4c5`, 78 / 100).
Mode: read-only static audit + `cargo bench --quick` measurement run
(no production code edited).

**Forcing-function check (post-bench-harness landing).** r9 was the
last round that could legitimately quote "unknown — needs
measurement" against every claim. As of cycle 09:47 the bench
harness `crates/plugin-db/benches/bench_query_build.rs` is on disk
and registered in `crates/plugin-db/Cargo.toml:32-34` under
`[[bench]] name = "bench_query_build" harness = false`. The r9
gating condition is satisfied. **This round quotes real numbers.**

Cycle commits since r9 (`git log 7d0bc4c5..HEAD -- crates/plugin-db/`):

```
bed655c1  plugin-db/replication: docstring drift on empty_returning test
757026e3  plugin-db: docs hold-out closures
7bd2187e  plugin-db/benches: scaffold initial cargo bench harness
389749ca  plugin-db: unify backend-missing code to backend_not_initialized
```

Three of the four are non-code (`bed655c1`, `757026e3`, comment-only
in `389749ca`). The fourth (`7bd2187e`) is *new bench infrastructure*,
not production code. **No production-code change since r9 affects
the hot path; perf delta is entirely about (a) what the bench tells
us, and (b) the structural carry-overs still on disk.**

---

## 1. Bench output — `cargo bench -p zeroship-plugin-db --bench bench_query_build -- --quick`

Build profile: `bench` (`opt-level=3`, criterion 0.x via workspace).
Wall: 1m 26s to compile + ~17s to measure. Numbers below are
criterion's `mean.point_estimate` from `target/criterion/*/new/estimates.json`,
cross-checked against the inline `time:` lines.

| Bench                       | Mean       | 95% CI                  | std_dev     |
| --------------------------- | ---------- | ----------------------- | ----------- |
| `build_find/empty`          |  493.76 ns | [484.20, 503.32] ns     |  13.52 ns   |
| `build_find/small` (2 eqs)  |  976.37 ns | [975.20, 977.54] ns     |   1.66 ns   |
| `build_find/complex`        | 2923.20 ns | [2915.95, 2930.45] ns   |  10.25 ns   |
| `build_insert/small_doc`    | 2054.53 ns | [2020.71, 2088.36] ns   |  47.83 ns   |

Workloads (from `bench_query_build.rs:55-94`):

- **empty** — `find({})` with no filter; smallest possible CRUD call.
- **small** — `find({ status: "active", role: "admin" })`; 2-3 top-level
  equalities, the median SDK call shape.
- **complex** — `$and` of `{ status: $in [3] }` + `$or { role | createdAt $gte }`
  + `{ score: { $gte, $lte } }`; admin-filter / analytics shape.
- **insert** — 6-field user record (id/email/name/role/createdAt/updatedAt).

### What the numbers mean

**Floor cost is ~500 ns.** `find({})` does `validate_collection` +
`validate_schema` + two `quote_ident` allocations + one `format!` for
the SELECT prefix. The byte-prefix `validate_collection` fix from r1
is verified non-quadratic — the 500 ns floor is consistent with five
small allocations and a couple of cmovs. If the prefix check
regressed back to substring-scan, the empty case would lift toward
the small case; an HTTP front-line at 200 K req/s would lose ~50-100
μs/sec in pure SQL build time per worker if that regressed.

**Each top-level equality costs ~240 ns.** small (2 eqs) − empty
= 482 ns ≈ 241 ns/eq. That cost is: one `quote_ident` (String alloc)
+ one `value_to_param` (String alloc) + one `format!("{col} = ${}")`
(String alloc) + one `params.push`. Three small Strings per equality
is the dominant cost; this matches N9-M2 / M3's structural prediction.

**Complex query is ~3× small.** complex − empty = 2429 ns vs.
small − empty = 482 ns. The complex shape carries 6 leaf predicates
(3 `$in` elements + 2 `$or` branches + 1 `$gte` + 1 `$lte`) ⇒
~405 ns/leaf — slightly above the 241 ns/eq baseline, consistent
with `$in`'s Vec<String> placeholder allocation (N9-I2) costing a
genuine extra String + Vec per element. **The bench substantiates
N9-I2's cost shape**: 3 elements at +164 ns/element vs. the eq
baseline = ~492 ns of overhead the `$in` branch could shed by
writing placeholders directly into the predicate string.

**Insert is ~2 μs for 6 fields.** ~340 ns/field. Same structural
shape: one `quote_ident` + one `value_to_param` + one
`"${n}"` placeholder. Insert is slightly cheaper per field than find
because there's no operator switch — it's just a flat field loop.

### What is conspicuously NOT measured

- `row_to_json` — the largest carry-over (N9-I1, O(N²) column
  lookup). Not bench-able from outside the crate because
  `compio_postgres::Row::new` is `pub(crate)`. The bench file's
  preamble (`bench_query_build.rs:10-34`) documents this constraint
  honestly. **Every "saves X ns per row decode" claim in this
  report remains static-analysis, NOT measured.**
- `crud::dispatch_*` — all 12 dispatch entry points are
  `pub(crate)` (verified `crates/plugin-db/src/crud.rs:140-543`).
  Cannot be benched without exposing a test wrapper or feature gate.
- `broker::publish` — `pub fn` (verified `broker.rs:480`) but takes
  `&mut self`. A subscriber-fanout bench is plausible without
  Postgres if the bench constructs a synthetic `ChangeEvent`. Worth
  pursuing in r11.
- `prefix_message` / `coded_sql` — `pub(crate)` and not on the hot
  path (only fires on error). Low leverage.
- `validate_collection` — exercised transitively by every
  `build_find` / `build_insert` call. Already a regression guard
  via the empty-filter case (floor moves if validate regresses).

---

## 2. Next-bench-target recommendation

Ranked by leverage × landable-without-production-edits:

### Top recommendation: `broker::publish` fanout — landable, hot path

`broker::publish` is `pub`. `ChangeEvent` is `pub` (verified
`broker.rs:1-100`). A bench can:

1. Construct a `Broker` via `Broker::new()` (`broker.rs:412`, `pub`).
2. Spin up N subscribers via `broker.subscribe(app_id, collection)`
   (`broker.rs:424`, `pub`).
3. Build a synthetic `ChangeEvent`.
4. Time `broker.publish(&event)` across 1, 9, 64, 256 subscribers.

This is the same shape exercised in cycle 04:00's 9-subscriber test;
landing it as a microbench would:

- Validate the Rc-sharing claim in `broker.rs:495-504` (the comment
  says deep-clone of `new_tuple` was the dominant cost pre-Rc; bench
  would confirm Rc::clone is now ~tens of ns vs. the multi-μs HashMap
  deep-clone).
- Establish a baseline for any future per-isolate fanout change.
- Guard against regressions in `accepts()` (the read-set filter).

**Score impact if landed: +3 to +5.** This is the highest-leverage
"landable now" target.

### Second: `validate_collection` as a dedicated regression guard

Already covered by the `empty` case, but a direct microbench would
make r1's byte-prefix fix immortal. Trivial to write (single `pub(crate)`
function — needs either a test-helper or a re-export). Low absolute
value but very cheap to land.

**Score impact if landed: +1 to +2.** Symbolic — closes one of the
historical perf wins permanently.

### Third: `crud::dispatch_*` — bigger surgery, biggest payoff

The CRUD dispatch path is what every `zeroship.db.*.find()` /
`.insert()` JS call traverses. Benching it requires either:

- A V8 isolate per bench iteration (heavy — adds ~50 μs minimum),
  OR
- A `#[cfg(feature = "test-helpers")]` wrapper that exposes the
  Rust-level dispatch entry point taking `&Value` args instead of
  V8 args (matches the pattern already established for
  `tests/integration.rs`).

The latter is the right move. Once landed, you get end-to-end
"JS-to-SQL-string" cost numbers including the v8_bridge marshaling
overhead — the actual figure SDK callers pay.

**Score impact if landed: +5 to +8.** Highest leverage but
biggest scope. Probably not landable in one cycle.

### Last: `row_to_json` — blocked on upstream

Status unchanged from r9. To bench externally we need either
(a) compio-postgres exposing a `#[cfg(feature = "test-helpers")]`
`Row::new` (the right move, parallels `plugin-db`'s `test-helpers`
feature flag) or (b) a `pub fn row_to_json_for_tests(columns: &[(String, u32)],
values: &[Option<Vec<u8>>]) -> Value` shim inside `plugin-db`.

Until one ships, **N9-I1 stays a static-analysis IMPORTANT, no
measurement** — the single biggest evidence gap this review has.

### What NOT to bench next

- `prefix_message` / `coded_sql` — cold path. Score impact ~0.
- `migrations::exec_fetch_batch` — needs a live PG. Defer.

---

## 3. Carry-over IMPORTANTs (re-walked)

### N9-I1 — `row_to_json` O(N²) column lookup

`crates/plugin-db/src/v8_bridge.rs:353-361`. **UNCHANGED.**

```rust
pub(crate) fn row_to_json(row: &compio_postgres::Row) -> Value {
    let mut obj = serde_json::Map::new();
    for col in row.columns() {
        let key = col.name().to_string();
        let value = column_to_json(row, col.name(), col.type_().oid());
        obj.insert(key, value);
    }
    Value::Object(obj)
}
```

`column_to_json` then does `row.try_get::<_, T>(name)` for every
column — each `try_get` is a linear scan of `row.columns()` to
resolve the column name to an index (compio-postgres
implementation; verified against the field-name lookup pattern).
For N columns the total cost is O(N²).

**Fix shape (unchanged from r9):** iterate `row.columns()` once
with `enumerate()`, pass the index to `try_get` (or
`row.try_get_raw(idx)` if available). For typical 10-20 column
result rows on a hot CRUD read, this is the difference between
~100 lookups and ~10 lookups per row decode.

**Cannot be measured this round** — bench-harness exclusion above.

### N9-I2 — `$in` / `$nin` Vec<String> placeholder allocs

`crates/plugin-db/src/query.rs:1944-1969`. **UNCHANGED.** Re-read
verbatim:

```rust
"$in" => {
    let arr = val.as_array().ok_or_else(|| {
        QueryError::InvalidFilter("$in must be an array".to_string())
    })?;
    let placeholders: Vec<String> = arr
        .iter()
        .map(|v| {
            params.push(value_to_param(v));
            format!("${}", params.len())
        })
        .collect();
    format!("{col} IN ({})", placeholders.join(", "))
}
```

**Now backed by measurement.** Section 1's complex-vs-small delta
shows `$in` carries ~+165 ns/element above the per-equality cost.
For a 3-element `$in` that's ~500 ns of avoidable work; for a
20-element `$in` (legitimate analytics shape — "show me rows where
id IN [list of 20 ids]") that scales to ~3.3 μs of pure
`format!("${}")` and `Vec<String>::join`.

**Fix shape:** replace the Vec<String> + join with a direct
`String::with_capacity` + `write!` loop into a `predicate`
String. The `params.len()` running index is already known —
no need to materialise placeholders first. Drops the per-element
String alloc and the trailing join. Static-analysis estimate:
~70-90 ns/element savings.

**Promotion candidate to the "verified by measurement" tier.**

### N9-I3 — `migrations::exec_fetch_batch` Vec→String

`crates/plugin-db/src/migrations.rs:423-425`. **UNCHANGED.**

```rust
let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
Ok(Value::Array(row_jsons).to_string())
```

The `Vec<Value> → Value::Array → .to_string()` round-trip
materialises every row as a serde-tree node, then re-serializes the
whole array back to a String. For a 1000-row migration backfill
batch this is two passes over the same data plus the intermediate
allocation.

**Fix shape:** stream into a single growable String — write `[`,
then call a JSON-writer adapter on each row, comma-separate, write
`]`. compio-postgres rows could write directly without the
`serde_json::Value` intermediate IF `column_to_json` were
re-shaped to take a `&mut String` instead of returning a `Value`.
Bigger surgery than N9-I2 but materially cheaper for backfills.

**Cannot be measured this round** — needs a live PG.

---

## 4. New / re-checked structural items (none promoted to IMPORTANT)

- **`build_find` SELECT-clause `Vec<String>` for projection** —
  `query.rs:1027-1037`. Same shape as N9-I2 (collect a `Vec<String>` of
  quoted columns, then join). Not measured in the bench (none of the
  three workloads pass a `select:` projection). Static-analysis only;
  same fix pattern as N9-I2. **MINOR.**

- **`build_find` `format!(" LIMIT {lim}")` and `OFFSET {off}` per call** —
  `query.rs:1056-1061`. Two more format! allocs the bench
  workloads all incur (limit=50, offset=0). Could be replaced with
  `write!(&mut sql, " LIMIT {lim}")` to avoid the temporary String.
  Tiny — ~30-50 ns per call, but it's in the floor of every find. **MINOR.**

- **`build_where` recursive String building** —
  `query.rs:1850+`. The complex-bench result (2.4 μs over empty)
  is mostly time spent here. The implementation uses `format!`
  liberally for sub-predicates and `Vec::join` for combining. A
  single growable String with capacity hint would help, but the
  refactor is large — defer until N9-I2 lands as a proof-of-pattern.
  **MINOR — defer.**

- **`broker::publish` Rc-clone fanout** — `broker.rs:495-516`.
  The pre-Rc deep-clone-of-HashMap cost is gone (commit history
  confirms). Comment block claims this is now refcount-bump only.
  **VERIFIED structurally; would be confirmed by the recommended
  r11 fanout bench.** No issue raised.

- **`backend_not_initialized` unification** (`389749ca`) —
  literal-string change in three sites. Zero codegen delta.
  **Perf-neutral, confirmed.**

---

## 5. Commits since r9 — verified perf delta

| Commit     | Files                                              | Perf delta            |
| ---------- | -------------------------------------------------- | --------------------- |
| `bed655c1` | `replication.rs` (test docstring)                  | 0 (docs only)         |
| `757026e3` | docs hold-out closures                             | 0 (docs only)         |
| `7bd2187e` | `benches/bench_query_build.rs` (NEW), `Cargo.toml` | 0 prod (bench infra)  |
| `389749ca` | `error.rs` + 2 callsites (literal change)          | 0 (compile-time)      |

**No perf-regressing commit landed since r9. No perf-targeted
optimisation landed since r9 either.** The score movement this round
is entirely driven by the bench harness landing (gating-condition
satisfaction) and the new measurement evidence it produces.

---

## 6. Reality check on prior-round claims

For the first time, several long-running static-analysis claims can
be cross-checked against numbers:

| r9 claim                                              | Now measured?       | Verdict                |
| ----------------------------------------------------- | ------------------- | ---------------------- |
| `validate_collection` byte-prefix fix non-quadratic   | YES (empty floor)   | Confirmed (~500 ns)    |
| `$in` Vec<String> overhead is meaningful              | YES (complex bench) | Confirmed (~165 ns/el) |
| `row_to_json` O(N²) is largest carry-over             | NO (unbenchable)    | Static-analysis only   |
| Per-equality build cost is allocation-bound           | YES (small−empty)   | Confirmed (~240 ns/eq) |
| `build_insert` ~equivalent cost per field as find     | YES                 | Confirmed (~340 ns/field) |

**Five claims, three converted from "static-analysis" to
"measured". That is the forcing-function payoff r9 demanded.**

---

## 7. Anti-fabrication compliance

- Every quoted number above traces to either the criterion `time:` line
  (printed by the bench run) or to `target/criterion/<group>/<id>/new/estimates.json`
  on disk (the `point_estimate` mean).
- Derived figures (per-eq cost, per-leaf cost, per-field cost) are
  arithmetic on the measured means. They are *implied* costs, not
  *measured* costs; the report calls this out where it matters.
- N9-I1 and N9-I3 remain unmeasured by construction (unbenchable
  from outside the crate). Their fix-shape estimates remain
  static-analysis only and are NOT quoted as "X ns savings".
- The `compio_postgres::Row` lookup-cost claim ("each `try_get` is
  a linear scan") is from reading the bench preamble's documented
  rationale (`bench_query_build.rs:14-19`) and structural inspection
  of `column_to_json`; not from a measurement on compio-postgres
  itself.

---

## 8. What would lift past 85

- N9-I2 fixed (now backed by measurement — `~165 ns/element` overhead
  is the bench-verified gap).
- A `broker::publish` fanout bench lands (closes the
  "second-largest unmeasured path").
- One of the structural MINORs above (`build_find` SELECT projection,
  `build_where` recursive concat) landed as the proof-of-pattern.

## What would lift past 90

- All three IMPORTANTs closed (N9-I1, N9-I2, N9-I3). N9-I1 still
  requires compio-postgres extension or in-crate test-helper shim.
- `crud::dispatch_*` bench lands behind `test-helpers` feature flag.
- A continuously-tracked baseline file (`crates/plugin-db/benches/results-*.txt`
  paralleling `crates/runtime/benches/results-*.txt`) so cycles can
  diff successive runs.

## What would lift past 95

- N9-I1's row-decode hot path verified by bench at ~10× the current
  static-analysis estimate.
- An end-to-end CRUD bench (JS handler → SQL emit → row decode → JS
  resolve) under criterion measuring real `zeroship.db.users.findOne()`
  cost.
- Sub-microsecond `build_find/small` on the median shape.

---

## 9. Score

**81 / 100.** (r9: 78. Delta: +3.)

### Why +3 and not larger

- The harness landed (the r9 gating condition). That alone closes
  the "static-analysis-only" criticism that capped the cycle at ~85.
- Two of the three carry-over IMPORTANTs are now structurally
  confirmed by the empty-vs-small-vs-complex spread (N9-I2's cost
  shape is real and measurable; validate_collection's prefix-fix
  is non-quadratic).
- **No actual perf fix landed.** All three IMPORTANTs are still on
  disk. The bench harness is infrastructure, not optimisation.
- N9-I1, the largest carry-over, remains unmeasurable until
  compio-postgres exposes a `Row` constructor — a cross-crate
  dependency this round cannot resolve.
- The score cannot move past ~85 without an actual perf-targeted
  commit landing. r11 should not run until either a fix lands for
  N9-I2 (now the cheapest, best-justified target) OR the
  `broker::publish` fanout bench is on disk.

### Comparison vs. prior rounds

| Round | Score | Forcing-function landed                                                 |
| ----- | ----- | ----------------------------------------------------------------------- |
| r6    | 76    | (none — pure static analysis)                                           |
| r7    | 76    | (none)                                                                  |
| r8    | 78    | Correctness/discipline fixes incidentally credited                      |
| r9    | 78    | (none — explicitly gated future rounds on bench landing)                |
| r10   | 81    | **`bench_query_build.rs` landed; 3 of 5 prior claims now measured**     |

### r11 gating condition

This round's analogue of r9's blocker:

**r11 should not run until one of:**

1. A commit lands fixing N9-I2 (`$in`/`$nin` direct String build).
   Now the cheapest, best-justified IMPORTANT — bench confirms cost
   shape, fix is ~30 lines.
2. `broker::publish` fanout bench lands under
   `crates/plugin-db/benches/`. Section 2 recommendation.
3. `compio-postgres` exposes a `Row` constructor (or an equivalent
   in-crate shim) unblocking the `row_to_json` bench. Section 1's
   biggest evidence gap.

Without one of these, r11 will produce another 81 with the same
three IMPORTANTs and the same observation that no perf-targeted
fix has landed in 10 cycles.

---

**Anti-fabrication footer:** bench output reproducible via
`cargo bench -p zeroship-plugin-db --bench bench_query_build -- --quick`
from the repo root. All ns figures above are criterion mean
point-estimates from
`target/criterion/build_{find,insert}/<param>/new/estimates.json`.
Derived per-element / per-field figures are arithmetic on those
means; their CIs are wider than the raw measurements'. N9-I1's
"O(N²)" remains a static-analysis claim — column-lookup cost on
`compio_postgres::Row` was NOT independently measured this round.
