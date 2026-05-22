# plugin-db Performance Review — 2026-05-22 r9

Commit: HEAD (`7d0bc4c5`). Cycle baseline: r8 (`f6043126`, 78 / 100).
Mode: read-only, no benchmarks executed.

**Bench reality check (anti-fabrication baseline).** Re-verified:

- `ls /home/ruiyang/Projects/appbase/crates/plugin-db/benches` → directory
  does not exist (no `cargo bench` harness for the DB path).
- `crates/runtime/benches/results-*.txt`: 21 files, newest still
  `results-2026-05-10-after-eternal-sweep.txt`. A keyword grep for
  `plugin-db|plugin_db|crud|findOne|insertOne|zeroship\.db|db_find|db_insert`
  across all of them returns zero matches.

Every quantitative claim below is therefore a static-analysis count
annotated **`unknown — needs measurement`**. No ns/% / speedup numbers
are quoted from memory or fabricated.

Commits since the r8 baseline
(`git log f6043126..HEAD -- crates/plugin-db/`):

```
7d0bc4c5  plugin-db/exec: unify cold-init failure code to lazy_init_failed (R8-2)
9e392ba1  plugin-db/error: accurate enumeration of Result<_, String> hold-outs (R8-1)
bc4363f0  plugin-db/replication: demote OBJECT_PREFIX to pub(crate) (re-apply)
09e32998  plugin-db: scrub stale TX_CONN/TX_TOKEN/MIG_LOCK refs + demote OBJECT_PREFIX
3d79d2da  plugin-db: docs-audit r6 fixes — coded_db preamble + 2 IMPORTANT drift sites
f1c5184e  plugin-db/error: add hint field to DbError::Configuration
```

(The task brief listed five; HEAD has six. `7d0bc4c5` landed after the
brief was drafted and is included in this audit.)

---

## 1. Verify cycle commits' perf-neutrality

The task brief states: **only `f1c5184e` Configuration-hint adds
allocation; cold-path only.** Verified per commit:

### 1.1 `f1c5184e` — `Configuration { hint: Option<String> }`

**Files:** `error.rs:120-127, 264-266, 296-322`; 5 construction sites
in `postgres.rs:593`, `wal_consumer.rs:349`, `replication.rs:277`,
`orchestrator/register_model/mod.rs:119, 126`.

Cost shape:

- The enum variant now carries one extra `Option<String>` field
  (24 bytes inline — `Option<String>` is niche-optimised to the size
  of a `String`). No per-call alloc on `None` construction (the three
  invariant-class sites pass `hint: None`); on `Some(hint)` (the two
  `wal_level_not_logical` and `CIC budget` sites) one additional
  `String` is allocated to carry the operator-remediation prose.
- Every site is cold-path: lazy init failure, register_model lazy
  init, replication startup wal_level check, wal_consumer startup
  db_url empty, postgres backend CIC budget. Walked all 5 call sites
  via `grep DbError::Configuration|config_hinted` — zero hits in
  `crud.rs`, `query.rs`, `exec.rs::run_sql`, `v8_bridge.rs`, or the
  WAL `Insert`/`Update`/`Delete` apply path (`wal_consumer.rs::emit_for_tuple`).

**Status: perf-neutral on hot paths. Cold-path-only 1× extra String
alloc per Configuration ERROR that carries a hint.** Confirms the
brief.

Verification: `crates/plugin-db/src/error.rs:120-127, 264-266,
296-322`; `grep "DbError::Configuration\|DbError::config\|config_hinted"
crates/plugin-db/src/` (10 hits, all in non-hot paths). Bench: unknown
— needs measurement (cold path; would not show in a CRUD bench
anyway).

### 1.2 `9e392ba1` — error.rs preamble enumeration of `Result<_, String>` hold-outs

**File:** `crates/plugin-db/src/error.rs:32-78` (doc-comment block).

`git show 9e392ba1 --stat` reports `1 file changed, 31 insertions(+),
12 deletions(-)` — entirely additions/edits to the **module-level
doc-comment preamble**. No code changes.

**Status: perf-neutral (test- and behavior-neutral).** Confirms the
brief.

Verification: `git show 9e392ba1 -- crates/plugin-db/src/error.rs`
shows only `//!` and `///` line edits.

### 1.3 `09e32998` + `bc4363f0` — TX_CONN sweep + OBJECT_PREFIX demote

**Files (09e32998):** `backend/mod.rs:5`, `crud.rs:5`, `exec.rs:3`,
`lib.rs:8`, `orchestrator/transaction.rs:13`,
`v8_classes/migration.rs:5`, `v8_classes/transaction.rs:46`.
**File (bc4363f0):** `replication.rs:1` (visibility change
`pub OBJECT_PREFIX` → `pub(crate) OBJECT_PREFIX`).

`09e32998` is a doc-comment / inline-comment sweep: replacing stale
references to the retired thread-local names (`TX_CONN`, `TX_TOKEN`,
`MIG_LOCK`) with the current `IsolateDbContext::tx_conn` / `tx_token`
/ `mig_lock` field paths. Zero behavioural changes.

`bc4363f0` is a one-keyword visibility demotion (`pub` → `pub(crate)`)
on a string constant. Rust visibility modifiers do not affect
generated machine code; the constant remains the same value at the
same address.

**Status: both perf-neutral.** Confirms the brief.

Verification: `git show 09e32998 -- crates/plugin-db/src/exec.rs`
(3-line diff, all comment edits); `git show bc4363f0 --stat` (1
line changed: `pub` → `pub(crate)`).

### 1.4 `3d79d2da` — docs fixes

**Files:** `error.rs:336-341`, `migrations.rs:67-86`,
`wal_consumer.rs:49-51`. `git show 3d79d2da --stat` reports `3 files
changed, 25 insertions(+), 26 deletions(-)` — three inline doc fixes
(preamble for `coded_db`, module preamble for `wal_consumer`, helper
enumeration for `prefix_message`).

**Status: perf-neutral (docs-only).** Confirms the brief.

### 1.5 `7d0bc4c5` — code-unify `lazy_init_failed` (NOT in the brief)

**File:** `crates/plugin-db/src/exec.rs:64, 317`.

Two `DbError::config("not_configured", ...)` calls in `run_sql` and
`ensure_pool` switched to `DbError::config("lazy_init_failed", ...)`.
Both wrap the same `init_pool_async().await` failure. The `code`
field on `DbError::Configuration` is a `&'static str` — the literal
changed but the type, size, and alloc shape are identical. The
`format!("db: lazy init failed: {e}")` body is unchanged.

**Status: perf-neutral.** Just a string-literal swap to give the SDK
one canonical wire code for cold-init failures.

Verification: `git show 7d0bc4c5 -- crates/plugin-db/src/exec.rs`
(2-line diff, identical alloc shape).

---

## 2. Carry-over IMPORTANTs — re-verified at HEAD

### N9-I1 (was N8-I1) — `row_to_json` O(N²) column lookup

**File:** `crates/plugin-db/src/v8_bridge.rs:353-361`.

Verbatim at HEAD (`Read` lines 353-361):

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

Each `column_to_json` (`:364-497`) dispatches via
`row.try_get::<_, T>(col.name())` — `compio-postgres`'s
`RowIndex for str` impl is a two-pass linear scan (case-sensitive
then case-insensitive). For N columns × O(N) per lookup → **O(N²)
per row decode**.

  [IMPORTANT] crates/plugin-db/src/v8_bridge.rs:353-361 — O(N²) column lookup on every row decode
    Why: largest remaining structural cost on every CRUD read.
      Quadratic in column count — at 20 columns this is 400 string
      compares per row vs. 20 with index dispatch. Fires on every
      `findOne` / `find` / `aggregate` returned row.
    Fix: pass `usize` indices down to a `column_to_json_by_idx`
      variant that calls `row.try_get::<usize, T>(idx)`. Single-file
      change; also folds in N9-M5 (`Map::with_capacity(N)`).
    Verification: `v8_bridge.rs:353-497` verbatim at HEAD (Read
      offset 350 limit 150). `git diff f6043126..HEAD --
      crates/plugin-db/src/v8_bridge.rs` → empty. Bench: unknown —
      needs measurement (a `db_find_wide_table` against a 20-col
      table would quantify).

**Status: STILL OPEN. Unchanged since r3 (10 rounds across r3-r9).**

### N9-I2 (was N8-I2) — `$in` / `$nin` placeholder `Vec<String>`

**File:** `crates/plugin-db/src/query.rs:1944-1968`.

Verbatim at HEAD:

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
"$nin" => { /* identical pattern at :1957-1968 */ }
```

Per N-element `$in`: **N small `String`s + 1 `Vec<String>` + 1
`join` String + 1 outer `format!` String = N + 3 allocs.**

  [IMPORTANT] crates/plugin-db/src/query.rs:1944-1968 — $in / $nin placeholder allocates N+3 Strings
    Why: hot on `find({ id: { $in: [...] } })`, the canonical
      "load these by ID" pattern. Scales linearly with the array
      size, so the cost grows with the use case.
    Fix: stream into one buffer via `std::fmt::Write`:
      ```rust
      let mut buf = String::with_capacity(arr.len() * 4 + col.len() + 8);
      write!(buf, "{col} IN (").unwrap();
      for (i, v) in arr.iter().enumerate() {
          params.push(value_to_param(v));
          if i > 0 { buf.push_str(", "); }
          write!(buf, "${}", params.len()).unwrap();
      }
      buf.push(')');
      ```
      One alloc regardless of array length.
    Verification: `query.rs:1944-1968` verbatim at HEAD (Read
      offset 1940 limit 35). `git diff f6043126..HEAD --
      crates/plugin-db/src/query.rs` → empty. Bench: unknown —
      needs measurement (a `db_find_in` with array sizes 10/100/1000
      would quantify).

**Status: STILL OPEN. Unchanged since r3.**

### N9-I3 (was N8-I3) — `migrations::exec_fetch_batch` `Vec` → `String` round-trip

**File:** `crates/plugin-db/src/migrations.rs:423-425`.

Verbatim at HEAD:

```rust
let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
Ok(Value::Array(row_jsons).to_string())
```

The returned `String` is then handed back to V8, which re-parses it
into a JS value.

  [IMPORTANT] crates/plugin-db/src/migrations.rs:423-425 — Vec<Value> serialised to String then re-parsed in V8
    Why: cold path (backfill batches at deploy time, ≤10 000 rows
      per call, not per CRUD), but structurally wasteful: one
      `serde_json::to_string` pass and one `JSON.parse` pass per
      batch.
    Fix: change `exec_fetch_batch`'s return type to `Vec<Value>`
      matching `exec_query`; let the v8_class wrapper hand it
      through to V8 via `ResolveValue::Json` (one serialise at the
      boundary, no round-trip).
    Verification: `migrations.rs:423-425` verbatim at HEAD (Read
      offset 418 limit 20). `git diff f6043126..HEAD --
      crates/plugin-db/src/migrations.rs` shows only the preamble
      doc edits from 3d79d2da; the function body is byte-identical
      to r8. Bench: unknown — needs measurement.

**Status: STILL OPEN. Unchanged since r3. Severity: IMPORTANT (cold
but structural).**

---

## 3. CRUD hot path — fresh re-walk

No structural change to `crud.rs` / `exec.rs` / `query.rs` /
`v8_bridge.rs` since r8. The six cycle commits touched:

- the `Configuration` enum variant (cold-path, §1.1);
- module-level doc-comments (`error.rs`, `migrations.rs`,
  `wal_consumer.rs`, several files in 09e32998's sweep);
- a `pub` → `pub(crate)` visibility change;
- a `&'static str` literal swap on a cold-path error code.

`git diff f6043126..HEAD -- crates/plugin-db/src/crud.rs` → empty.
`git diff f6043126..HEAD -- crates/plugin-db/src/query.rs` → empty.
`git diff f6043126..HEAD -- crates/plugin-db/src/v8_bridge.rs` →
empty.

The findOne / insertOne / updateMany alloc counts walked in r8 §3
hold byte-for-byte: ~7 Strings + 1 Map + 1 Vec + ~16 string compares
per `findOne` on a 4-col row; 2 String moves into the dispatch
closure per write; ~1200 small Strings per 100-row × 5-col
updateMany when subscribers exist.

**Status: identical to r8.** Re-walking does not surface anything
not already captured by N9-I1/I2/M1-M10.

---

## 4. WAL consumer hot path — fresh re-walk

`crates/plugin-db/src/wal_consumer.rs:486-646` verbatim at HEAD;
`git diff f6043126..HEAD -- crates/plugin-db/src/wal_consumer.rs`
shows only the docs-fix from 3d79d2da (a module preamble rename)
and the Configuration-hint addition at `:349` (cold-path error
construction at consumer startup). The hot `apply_tuple_change` /
`emit_for_tuple` path (`:486-612`) is byte-identical.

Re-verified inline at `:583-612`:

- `has_subscribers(&self.app_id, &rel.table)` gate at `:583` —
  unsubscribed tables return zero-alloc.
- Subscribed-path allocations: 1 `Vec<String>` + N column-name clones
  (`changed_columns`, N9-M1), 1 `HashMap` with `with_capacity` + per-column
  key/value clones (`new_tuple` via `tuple_to_map`, N9-M6), 2 String
  clones (`app_id` + `collection`, N9-M3), 1 `Rc::new` + 1 HashMap
  clone in `broker::publish` (§5).

≈ 20 allocs per WAL frame on a subscribed 5-col INSERT. Identical
shape to r8.

**Status: bounded, unchanged. No regression from cycle commits.**

---

## 5. Broker fanout — bounded check

`broker::publish` at `broker.rs:480-529`: `git diff
f6043126..HEAD -- crates/plugin-db/src/broker.rs` → empty. Identical
to r8 §4.

- Two-level lookup via `Borrow<str>` (`event.app_id.as_str()` → inner
  map → `event.collection.as_str()`) — O(1), no alloc.
- `subs.retain(|s| !s.is_closed())` — O(subs_on_this_collection).
- `Rc::new(event.clone())` at `:504` — one HashMap clone (the event
  payload) per publish total; subscribers share via `Rc::clone`
  (refcount bump only).
- Empty-bucket pruning at `:523-528` — per-collection Vec dropped
  when empty; per-app inner map dropped when its last collection
  emptied.

**Total: O(subs × predicate_entries), bounded by user-controlled
fan-out.** No regression.

---

## 6. `coded_sql` 3-alloc wrapper chain — re-verified

§6 of r8 (the 5 module-prefix wrappers above the shared
`error::coded_sql`) re-verified verbatim at HEAD:

```
audit.rs:58              fn coded_sql(...) { crate::error::coded_sql(&format!("audit: {context}"), e) }
auth/bootstrap.rs:24     fn coded_sql(...) { crate::error::coded_sql(&format!("auth/bootstrap: {context}"), e) }
auth/keys.rs:41          fn coded_sql(...) { crate::error::coded_sql(&format!("auth/keys: {context}"), e) }
auth/session.rs:35       fn coded_sql(...) { crate::error::coded_sql(&format!("auth/session: {context}"), e) }
diff.rs:40               fn coded_sql(...) { crate::error::coded_sql(&format!("diff: {context}"), e) }
```

Central helper at `error.rs:400-404`:

```rust
pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err: DbError = e.into();
    prefix_message(&mut err, &format!("{context}: "));
    err
}
```

Three Strings per error path: wrapper `format!`, central `format!`,
`prefix_message`'s `format!`. **Unchanged.**

  [MINOR] crates/plugin-db/src/{audit,auth/bootstrap,auth/keys,auth/session,diff}.rs — 3-alloc wrapper chain on every classified SQL error
    Why: 3 Strings vs. 1 pre-dedup. Cold path (only fires on SQL
      errors), but a clear discipline gap. The deeefe18 cleanup
      collapsed `migrations::coded_db` into the central helper, but
      the 5 module-prefix wrappers still pay the 3-alloc tax.
    Fix: add a 2-arg `coded_sql_with_module(module, ctx, e)` that
      composes module + context + message in a single `format!`;
      the 5 wrappers collapse to 1-line shims. Net: 1 alloc per
      error path.
    Verification: 5 wrapper grep hits + central helper at
      `error.rs:400-404`. Bench: unknown — needs measurement.

**Status: STILL OPEN. Unchanged since r6.** Severity: MINOR.

---

## 7. Other carry-over MINORs — quick re-verification

All re-checked via `Read` / `Grep` at HEAD; **byte-identical to r8**:

| ID | File:line | Status |
| --- | --- | --- |
| N9-M1 | `wal_consumer.rs:594-598` (changed_columns) | open, unchanged |
| N9-M2 | `crud.rs:218-219, 246-247, 279-280, 308-309, 340-341, 368-369, 514-515, 553-554` (8 sites × 2 String dispatch clones) | open, unchanged |
| N9-M3 | `wal_consumer.rs:603-611` (app_id + collection clones) | open, unchanged |
| N9-M4 | `exec.rs:189-249` (per-row tuple HashMap) | open, unchanged |
| N9-M5 | `v8_bridge.rs:354` (`Map::new()` without capacity) | open, unchanged; folds into N9-I1 fix |
| N9-M6 | `wal_consumer.rs:638` (`"NULL".to_string()` per NULL column) | open, unchanged |
| N9-M7 | §6 above (`coded_sql` 3-alloc) | open, unchanged |
| N9-M8 | `query.rs::value_to_param_inner` (clones every `Value::String`) | open, unchanged |
| N9-M9 | `crud.rs:403-411` (`dispatch_aggregate` fallback `Value::Object`) | open, unchanged |
| N9-M10 | `query.rs::build_aggregate` (unconditional `agg_exprs` HashMap) | open, unchanged |

`git diff f6043126..HEAD -- crates/plugin-db/src/{wal_consumer,exec,broker,query,crud,v8_bridge}.rs`
— only `wal_consumer.rs` has any change (the cold-path Configuration
construction at `:349`), confirming the table.

---

## 8. Plateau check

**Are there actionable performance items left? YES, but with a sharp
caveat.**

The actionable list has not changed since r3 in shape:

1. **N9-I1** — `row_to_json` O(N²) column lookup (the only IMPORTANT
   on the genuine hot path; structural and worth fixing).
2. **N9-I2** — `$in` / `$nin` `Vec<String>` (small, trivial,
   user-visible at large arrays).
3. **N9-I3** — `migrations::exec_fetch_batch` round-trip (cold but
   structural).

Plus ~10 MINORs (M1-M10) of varying cost, most on either the WAL
hot path (M1, M3, M6) or the broker-subscribed path (M4) or pure
cold-path discipline gaps (M7).

**What HAS plateaued is the cycle's marginal yield.** Six commits
landed since the r8 baseline; all six are either docs/comment-only,
SDK-surface tightening (the `lazy_init_failed` code unification), a
diagnostic-shape change (the Configuration hint slot), or a
visibility demotion. **Zero of the six commits moved a single
allocation off any path.** The cycle is now producing
correctness/clarity/diagnostic-surface improvements at the expense
of perf items being effectively static.

The three carry-over IMPORTANTs (N9-I1, N9-I2, N9-I3) have now been
in the report for **6 consecutive rounds** (r3 → r9) with no
material change. They are not stuck because the analysis is wrong;
they are stuck because **without a bench harness the cycle has no
forcing function to prioritise them over the 6-commit/round
correctness work.**

---

## 9. New findings — fresh re-walk

After re-walking CRUD dispatch (§3), WAL apply (§4), broker fanout
(§5), and error-path wrappers (§6) from scratch at HEAD:

### CRITICAL — none

No CRITICAL regressions. The six cycle commits are
diagnostic/typing/docs work; their hot-path impact is zero.

### IMPORTANT — none new

The three carry-overs (N9-I1, N9-I2, N9-I3) remain the only
IMPORTANT-class items. No new IMPORTANT raised.

### MINOR — none new

The MINORs list is unchanged. No new MINOR raised — the cycle's six
commits did not introduce a new allocation pattern.

---

## 10. Top-3 next-priority items (unchanged from r8)

1. **N9-I1** — `row_to_json` O(N²) column lookup. Single-file change
   in `v8_bridge.rs`; the largest remaining structural read-path cost.
   Also folds in N9-M5 (`Map::with_capacity(N)`).
2. **N9-I2** — `$in` / `$nin` placeholder `Vec<String>`. Trivial
   one-spot fix via `std::fmt::Write`.
3. **N9-M4** — per-row tuple HashMap in `emit_for_rows`. Scales with
   `affected_rows × columns`. Larger refactor (touches broker accept
   contract).

Cheap buyback (carry-over):

4. **N9-M9** — `dispatch_aggregate` fallback `Value::Object` builds
   even when no read-set capture is active. ~5-line wrap in
   `if read_set::is_active() { ... }`.
5. **N9-M7** — collapse the 5 `coded_sql` wrappers + central helper
   into a 2-arg variant; 1 alloc per SQL error path vs. 3.

---

## 11. Score

**78 / 100** (vs. r8's 78, r7's 76, r6's 76, r5's 76, r4's 74, r3's
70, r2's 57, r1's 44).

Movement vs r8: **+0**.

Per-commit accounting:

- **+0 for `f1c5184e`** (Configuration hint field): cold-path
  diagnostic improvement. No hot-path alloc change.
- **+0 for `9e392ba1`** (error.rs preamble enumeration): docs-only.
- **+0 for `09e32998` + `bc4363f0`** (TX_CONN sweep +
  OBJECT_PREFIX demote): docs + visibility change. Visibility
  modifiers don't change generated code.
- **+0 for `3d79d2da`** (docs fixes): preamble edits.
- **+0 for `7d0bc4c5`** (lazy_init_failed code unify): one
  `&'static str` literal swap on the cold init-failure error path.

Total: +0 → **78 / 100**.

What's still pulling below 80:

- **N9-I1** — O(N²) column lookup on every CRUD read decode.
- **N9-I2** — `$in` placeholder Vec allocation.
- **N9-I3** — migrations backfill Vec→String round-trip.
- The continuing absence of a `cargo bench --bench db_*` harness.
  Every round writes "unknown — needs measurement"; without numbers,
  the cycle cannot rank the MINORs against each other or against the
  IMPORTANTs.

What would lift past 85:

- N9-I1 fixed (index-based row decode); also folds in N9-M5.
- N9-I2 fixed (trivial).
- One DB-touching bench landed under `crates/plugin-db/benches/`.

What would lift past 90:

- All three IMPORTANTs (N9-I1, N9-I2, N9-I3) closed.
- The N9-M2 / M3 path (`Arc<str>` for `app_id` / `collection` across
  CRUD + WAL + broker) refactored. Cuts per-event allocation by ~3-4
  Strings on the WAL hot path; closes 10 dispatch sites in `crud.rs`.
- A documented latency-vs-throughput baseline (μs / op) for the CRUD
  operations so future cycles can detect regressions.

---

## 12. Honest assessment — diminishing-returns threshold

**The cycle has been producing zero-perf-delta rounds since r6
(scores: 76 → 76 → 78 → 78 over r6 → r7 → r8 → r9, with r7→r8 the
only positive movement, +2 from two error-path discipline wins that
were themselves correctness-driven).**

Three perf rounds in a row (r7, r8 partial, r9) have produced no
score movement attributable to a perf-targeted change. The +2 in r8
came from `e5315083` and `f6043126` — both of which were primarily
**correctness/discipline** work (substring-classification →
SQLSTATE-typed checks, dead-code cleanup) that the perf review
happened to credit. **No commit in the last 9 cycles was driven by
a perf finding raised in this review.**

**Recommendation: further static-analysis perf review is
counterproductive below ~85 / 100 without first landing a
`cargo bench` harness.** The current ceiling for this report is
~82-85 — that's where the static-analysis evidence is strong enough
to motivate the three IMPORTANT fixes if someone has the bench data
to justify the diff churn. Above that, every claim becomes a
"would-save N allocs in a microbenchmark we haven't run" assertion
that is impossible to either prioritise or falsify.

**Concrete threshold for diminishing returns: at the current 78,
the next perf round (r10) should not run unless one of the
following is true:**

1. A `cargo bench --bench db_findone` (or similar) lands under
   `crates/plugin-db/benches/`. Even a 30-line bench against a
   testcontainer-backed table converts N9-I1 from "static-analysis
   O(N²)" to "X ns/op at 4 columns, Y ns/op at 20 columns" — a
   number the cycle can rank against.
2. A cycle commit lands that explicitly closes N9-I1, N9-I2, or
   N9-I3 (a perf-targeted change to score against).
3. A new structural pattern is introduced in the DB code path (e.g.
   a new query operator, a new tuple-encoding path, a broker
   protocol change) that the static-analysis review can newly
   evaluate.

Without one of those forcing functions, **r10 will produce another
78 with the same three IMPORTANTs and the same ten MINORs**, and
that's not a useful round.

---

**Anti-fabrication reminder:** no plugin-db bench results exist in
`crates/runtime/benches/results-*.txt` (latest is
`results-2026-05-10-after-eternal-sweep.txt`; everything post-2026-04
exercises fetch/WS/RPC, not the DB path). No harness exists under
`crates/plugin-db/benches/`. Every "saves N allocs" / "−1 String"
claim in this report is a static-analysis count, not a measurement.
Score and severity are qualitative — no ns/% / speedup numbers
quoted. A bench landed would convert several of these from
assertions to numbers.
