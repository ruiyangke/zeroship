# plugin-db Performance Review — 2026-05-22 r3

Commit: HEAD (`309ed52f`) — head of `main` at audit time.
Scope: re-audit of FFI overhead, per-CRUD allocations, broker fanout, WAL consumer hot path, and schema-installation cost.
Constraint: read-only, no benchmarks run. The runtime bench suite under
`crates/runtime/benches/results-*.txt` does not exercise plugin-db
paths; no quantitative comparison vs. r2 is available.

---

## 1. Headline — Score Trajectory

**R1 (de01b3a0): 44 / 100 → R2 (c54a9f15): 57 / 100 → R3 (309ed52f): 70 / 100**

Both R1 CRITICAL serde round-trips are closed; both R1 IMPORTANT broker findings stay closed; R2's WAL-consumer CRITICAL is closed via the new `Broker::has_subscribers` gate. The remaining work is concentrated in:

1. A symmetric (and previously unflagged) waste in `exec_mutation_with_emit` — it still materialises `(columns, tuple)` per row before the `has_subscribers` check, mirroring R2 N-C1 but on the local-emit side.
2. A small allocation regression introduced by the new `Broker::has_subscribers` itself.
3. The query-builder `$in` / `$nin` placeholder vector (R2 N-I1, still open).
4. The backfill-path JSON-array round-trip (R2 N-I2, still open).
5. The `acquire_dedicated_client` detached connection task (R1 I3, still open) — out of scope for "per-call allocations" but flagged for completeness.

---

## 2. R1/R2 Closures Re-Verified

### CLOSED — R1 C1: Triple serde round-trip per read result

**Commit:** `cc7fff89` ("thread Vec<Value> end-to-end (drop serde round-trip)")

`crates/plugin-db/src/exec.rs:83–87` (`exec_query`) and `crates/plugin-db/src/exec.rs:112–116` (`exec_mutation`) both return `Vec<serde_json::Value>` directly. The intermediate `Value::Array(arr).to_string()` is gone. `crud.rs:105–108` (`first_row_or_null`) and `crud.rs:114–116` (`row_count_as_f64`) consume the owned `Vec<Value>` and serialise once at the V8 boundary (or take the count without serialising at all). The chain is now:

```
compio_postgres::Row
  → row_to_json (one serde_json::Map per row)
  → first_row_or_null: rows.into_iter().next().unwrap_or(Null).to_string()
  → ResolveValue::Json(string) → v8::json::parse
```

Net: 1 row decode → 1 String alloc → 1 V8 parse. Down from 4. The `crate::core::runtime.rs:2051–2054` `ResolveValue::Json` arm calls `v8::json::parse` once — verified in source.

### CLOSED — R1 C2: `exec_mutation_with_emit` re-parses its own output

**Commit:** `cc7fff89`

`exec.rs:148` reads `let rows = exec_mutation(bq).await?;` — `rows` is now `Vec<Value>`. The loop at `exec.rs:155–196` iterates the live `Value`s with no parse step. The `serde_json::from_str(&json)` line called out in R1 C2 is gone.

### CLOSED — R1 I1/I2: Broker `publish` Vec alloc + per-subscriber HashMap clone

**Commit:** `c54a9f15` (verified in r2; still in place at HEAD)

`broker.rs:438–462` prunes dead entries in place with `subs.retain(...)`, wraps `event` in a single `Rc::new(event.clone())`, and iterates the live bucket directly. No intermediate `Vec<Subscription>`; each `s.push(...)` carries a refcount bump rather than a `HashMap` deep clone.

### CLOSED — R1 I4: `AuditExecutor` boxes every future

**Commit:** `f1c475f5` (verified in r2; still in place at HEAD)

`audit.rs:431–438` uses `#[allow(async_fn_in_trait)] async fn` directly. `Pool` and `Client` impls at lines 440–458 are plain `async fn`. No `Pin<Box<dyn Future>>` in the trait.

### CLOSED — R2 N-C1: WAL consumer `emit_for_tuple` builds HashMaps before subscriber check

**Commit:** `967a7362` ("early-return in emit_for_tuple when no subscribers") + `78a95d3b` ("has_subscribers fast-path predicate")

`wal_consumer.rs:548–550` reads:

```rust
if !has_subscribers(&self.app_id, &rel.table) {
    return;
}
```

This sits before `changed_columns` is built (line 559) and before either `tuple_to_map` call (lines 565–566). The fix is correctly placed — the per-WAL-frame allocation is gone for tables with no subscribers. The conservative-true semantics (closed-but-not-yet-pruned subscribers count as "has subscribers") are documented at `broker.rs:472–485` and are correct: the cost of building one extra tuple is bounded.

---

## 3. New / Still-Open Findings

### CRITICAL

#### N3-C1 — `exec_mutation_with_emit` still builds `(columns, tuple)` per row before any subscriber check

**File:** `crates/plugin-db/src/exec.rs:155–196`

Symmetric to (and previously unflagged alongside) the R2 N-C1 finding in the WAL path. Every successful mutation runs:

```rust
for row in &rows {
    …
    let (columns, tuple): (Vec<String>, HashMap<String, String>) = match row {
        Value::Object(m) => {
            let cols = m.keys().filter(...).cloned().collect();    // alloc 1
            let tuple = m.iter().map(|(k, v)| {
                let s = match v { … };                              // alloc per val
                (k.clone(), s)                                       // alloc per key
            }).collect();                                            // alloc 2
            (cols, tuple)
        }
        _ => (Vec::new(), HashMap::new()),
    };
    queue_or_emit(app_id, collection, op, pk, columns, tuple);
}
```

`queue_or_emit` then either calls `emit_local` (which checks `is_app_suppressed` — true whenever a WAL consumer for this app is active — and short-circuits) OR `BROKER.publish` (which checks `by_key.get(...)` — short-circuits when no subscribers).

In production:
- The recommended deployment runs the WAL consumer (`replicationConsumerStart`) so cross-worker propagation works. `is_app_suppressed(app_id)` is then `true` for the writer's app and the entire `columns + tuple` materialisation feeds straight into a no-op.
- Even without the WAL consumer, the common-case mutation has zero subscribers on the affected collection. The HashMap is built and immediately discarded.

For an `insertMany` of N rows on a table with C declared columns: `N × (1 Vec<String> + C String clones + 1 HashMap + C key clones + C value allocs)`. For a typical `messages` insert of 5 columns, that's ~12 small allocations per row that the broker never reads.

**Why:** the work is positioned BEFORE the suppression / subscriber check, mirroring the exact mistake R2 N-C1 fixed in the WAL path.

**Fix:**
```rust
// Pre-flight: do we need a tuple at all?
let need_tuple = !context::with(|c| c.has_tx())                      // autocommit
    && !crate::wal_consumer::is_app_suppressed(app_id)               // local-emit path live
    && crate::broker::has_subscribers(app_id, collection);            // any reactive query

// Or, when in a tx, we DO need to queue (subscribers may attach
// by COMMIT time). Conservative-true: queue iff any subscriber
// exists at this moment AND we're in a tx.
let need_tuple = need_tuple
    || (context::with(|c| c.has_tx())
        && crate::broker::has_subscribers(app_id, collection));
```
And feed empty `Vec::new() / HashMap::new()` into `queue_or_emit` when `!need_tuple`. The `has_tx`/`is_app_suppressed`/`has_subscribers` calls are O(1) on thread-local data; the saving is N × tuple_columns allocations per mutation in the common case.

**Verification:** `exec.rs:155–196` body is unconditional. The only branch is `match row { Value::Object(m) => ..., _ => (Vec::new(), HashMap::new()) }` — the empty arms are reachable only when the row isn't an object (never, given `rows_to_json_value` always produces `Value::Object`). Severity is CRITICAL because the path runs on every successful write op and the saving is the same shape as N-C1 (which was rated CRITICAL).

### IMPORTANT

#### N3-I1 — `Broker::has_subscribers` allocates two `String`s on every call

**File:** `crates/plugin-db/src/broker.rs:486–490`

```rust
pub(crate) fn has_subscribers(&self, app_id: &str, collection: &str) -> bool {
    self.by_key
        .get(&(app_id.to_string(), collection.to_string()))
        .is_some_and(|v| !v.is_empty())
}
```

Each call allocates a fresh `String` for both `app_id` and `collection` to construct the `(String, String)` lookup key. The R2 fix put the predicate on the WAL consumer's hot path (`wal_consumer.rs:548`) — every pgoutput Insert/Update/Delete pays these two allocations even when no subscribers exist. That's at least 2 String allocs per WAL frame on a production worker.

**Why:** `HashMap::get` requires a key that implements `Borrow<K>`. For `(String, String)` the borrow form is `(&str, &str)`, but the std-`HashMap` doesn't auto-derive `Borrow<(&str, &str)>` for `(String, String)` — the canonical workaround is a `BorrowKey` wrapper or switching the key type. The current code took the simpler-but-allocating path.

**Fix options:**
1. Replace the inner map with `HashMap<(String, String), Vec<Subscription>>` → `HashMap<String, HashMap<String, Vec<Subscription>>>` (nested map; `.get(app_id).and_then(|m| m.get(collection))` borrows `&str` natively).
2. Define a `#[derive(Hash, Eq)]` `BorrowedKey<'a>(&'a str, &'a str)` plus a manual `Borrow` impl to look up without allocation. (Stable trick, see `hashbrown` docs.)
3. Switch to `hashbrown::HashMap` and use `raw_entry` for the lookup.

**Verification:** lines 486–490 verbatim contain `app_id.to_string()` and `collection.to_string()`. The same regression bites N-C1 callers — `has_subscribers` is called once per WAL Insert/Update/Delete (`wal_consumer.rs:548`) AND would be called once per local-emit if N3-C1 above is fixed.

#### N3-I2 — Query builder `$in` / `$nin` allocate a `Vec<String>` of placeholders per array element

**File:** `crates/plugin-db/src/query.rs:1946–1953` (`$in`), `1959–1966` (`$nin`)

```rust
let placeholders: Vec<String> = arr
    .iter()
    .map(|v| {
        params.push(value_to_param(v));
        format!("${}", params.len())
    })
    .collect();
format!("{col} IN ({})", placeholders.join(", "))
```

For each element of the array: one `format!` allocation for `"$N"` (a tiny `String`), one push into `placeholders: Vec<String>`. Then `placeholders.join(", ")` allocates a fresh `String` and frees the small ones.

A `{ id: { $in: [...20 elements...] } }` filter — common for "load these specific IDs" queries — allocates 20 small `String`s + the `Vec` + the joined string, vs. one streaming write into a single `String`. Unchanged since R1 M2 / R2 N-I1 — still in flight.

**Fix:** write directly into a `String` buffer using `std::fmt::Write`:

```rust
use std::fmt::Write as _;
let mut out = String::with_capacity(col.len() + 8 + arr.len() * 6);
let _ = write!(out, "{col} IN (");
for (i, v) in arr.iter().enumerate() {
    params.push(value_to_param(v));
    if i > 0 { out.push_str(", "); }
    let _ = write!(out, "${}", params.len());
}
out.push(')');
```

One allocation per `$in` clause regardless of array length.

**Verification:** `query.rs:1946–1953` and `1959–1966` verbatim contain the `Vec<String>` collect + `join`. No commit since R2 touches these lines.

#### N3-I3 — `migrations::exec_fetch_batch` materialises `Vec<Value>` then serialises to a `String` for the V8 boundary

**File:** `crates/plugin-db/src/migrations.rs:423–425`

```rust
let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
Ok(Value::Array(row_jsons).to_string())
```

The caller (`v8_classes/migration.rs`) receives the string and the runtime then parses it back via `v8::json::parse`. Same shape as R1 C1 — but on the backfill path, which only runs during migrations.

Unchanged since R2 N-I2.

**Why it stays IMPORTANT (not CRITICAL):** backfill batches are bounded (≤ 10 000 rows per `exec_fetch_batch` per `migrations.rs:360`) and only run during deploys. CRUD hot path is unaffected.

**Fix:** propagate the `Vec<Value>` to the V8 boundary the same way `exec_query` now does — change `exec_fetch_batch`'s return type to `Result<Vec<Value>, OpError>` and let the V8 callback build the JS array directly (or feed it through `ResolveValue::Json` with a single `Value::Array(rows).to_string()` at the boundary). One step in the chain disappears.

### MINOR

#### N3-M1 — `tuple_to_map` clones `col.name` and allocates `"NULL"` String for every NULL column

**File:** `crates/plugin-db/src/wal_consumer.rs:592–611`

```rust
TupleColumn::Null => {
    out.insert(col.name.clone(), "NULL".to_string());
}
```

`"NULL".to_string()` is a fresh allocation per NULL column on every WAL frame that passes the subscriber check. Unchanged since R2 N-M1. Minor because the upstream `has_subscribers` gate (closed N-C1) means this path only runs when subscribers exist.

**Fix:** intern `"NULL"` as a `static NULL_SENTINEL: &str = "NULL"`, change the value type to `Cow<'static, str>` or `Arc<str>`. Saves O(null-columns-per-frame × frames) allocations.

#### N3-M2 — `build_aggregate` unconditionally allocates `agg_exprs: HashMap<String, String>`

**File:** `crates/plugin-db/src/query.rs:1480`

```rust
let mut agg_exprs: std::collections::HashMap<String, String> = std::collections::HashMap::new();
```

The HashMap is only consumed when `$having` is in the pipeline. Pipelines without `$having` (the common case for `aggregate({$group: ..., $sort: ...})`) pay the allocation and free it unused. Unchanged since R2 N-M2.

**Fix:** `Option<HashMap<…>>` lazy-init on first `$having` write.

#### N3-M3 — `value_to_param` clones every `Value::String`

**File:** `crates/plugin-db/src/query.rs:2074`

```rust
Value::String(s) => s.clone(),
```

Called once per query parameter. For a 10-field insert, 10 String clones. Unchanged since R1 M1. The constraint is that `BuiltQuery.params: Vec<String>` needs owned strings to satisfy `query_text_params(sql, &[&str])`'s `'static`-or-borrowed lifetime split. Switching to `Cow<'_, str>` keyed off the borrowed `Value` tree would eliminate the clone but requires lifetime threading through `BuiltQuery`.

#### N3-M4 — `dispatch_*` write paths allocate two extra `String`s to move `app_id` / `collection` into the async block

**File:** `crates/plugin-db/src/crud.rs:218–219`, `247–248`, `280–281`, `309–310`, `341–342`, `369–370`, `515–516`, `552–553`

```rust
let coll = collection.to_string();
let app = app_id.to_string();
```

On every CRUD write call (insert / insertMany / updateOne / updateMany / deleteOne / deleteMany / upsert / findOrCreate) we move two owned `String`s into the future closure. Read paths (find / findOne / count / aggregate / distinct) don't have this because they don't need the app/collection inside the async block — only the write paths fan out to broker events.

**Why MINOR:** two short-lived allocations per write op. Unavoidable without changing `Collection`'s storage to `Rc<str>` (then `coll = Rc::clone(&self.name)` is a refcount bump only). 8 dispatch sites touched, one struct field change.

#### N3-M5 — `row_to_json` allocates `serde_json::Map::new()` without capacity hint

**File:** `crates/plugin-db/src/v8_bridge.rs:354`

```rust
let mut obj = serde_json::Map::new();
for col in row.columns() {
    let key = col.name().to_string();
    let value = column_to_json(row, col.name(), col.type_().oid());
    obj.insert(key, value);
}
```

`serde_json::Map` defaults to `BTreeMap` (or `IndexMap` with the `preserve_order` feature). Either way, `with_capacity(row.columns().len())` would skip rehashing for the common 8-12 column case. Trivial fix; runs on every row of every result set.

Also: `col.name().to_string()` allocates a fresh `String` per column. The column metadata is owned by the `Row` for the duration of the call — could reuse a `String` slice if `serde_json::Map` accepted `&str` keys (it doesn't; this is structural).

---

## 4. Still-Open Items from R1

- **R1 I3** (`acquire_dedicated_client` detached connection task at `backend/postgres.rs:76–82`): unchanged. `.detach()` still present. Latent FD/SQE-slot leak on rollback storms. Out of scope for "per-call allocations" but worth carrying forward.
- **R1 M1** (`value_to_param` Value::String clone): see N3-M3 above.
- **R1 M2** (`build_where` / `$in` nested `Vec<String>`): see N3-I2 above.
- **R1 M3** (`rows_to_json` materialises whole result set): now `rows_to_json_value` returns the `Vec<Value>` end-to-end; the result-set-to-string happens once at the V8 boundary via `rows_as_json_array`. Largely closed by the C1 fix; only the per-row `serde_json::Map` allocation remains, which is structural for the `Vec<Value>` intermediate.

---

## 5. Top Next-Priority Items

1. **Fix N3-C1** — gate `(columns, tuple)` construction in `exec_mutation_with_emit` behind a single `has_subscribers` + `is_app_suppressed` + `has_tx` check. Saves O(tuple_columns) allocations per row per mutation in the common production case where the WAL consumer is active. Single-file change in `exec.rs:155–196`.

2. **Fix N3-I1** — eliminate the two `String` allocations in `Broker::has_subscribers`. Either restructure the broker's `by_key` to a nested `HashMap<String, HashMap<String, ...>>` (idiomatic) or add a `BorrowedKey<'a>` wrapper (one-line change). This compounds with N3-C1 — if the gate is added on the local-emit side, the broker check fires twice per mutation; both should avoid allocating.

3. **Fix N3-I2** — replace `$in` / `$nin` `Vec<String>` placeholder build with `write!` into a pre-sized `String`. Single-spot, no API change.

These three fixes together cover the per-mutation allocation budget improvement that R2 partially addressed; N3-C1 in particular is the load-bearing CRITICAL.

---

## 6. Score

**70 / 100** (vs. R2's 57 / 100, R1's 44 / 100)

Two CRITICAL R1 items closed (C1, C2) and the R2 CRITICAL (N-C1, WAL consumer pre-allocation) closed. The hot read and write paths now thread `Vec<Value>` end-to-end with a single V8 boundary serialise — the dominant per-call cost from R1 is gone.

What pulls the score below 75 / 80:
- The new CRITICAL (N3-C1): `exec_mutation_with_emit` repeats the same mistake R2 N-C1 fixed in the WAL consumer, just on the local-emit side. Visible every time a mutation succeeds.
- The `Broker::has_subscribers` allocation regression (N3-I1): introduced by the R2 fix itself; cheap to repair but visible on every WAL frame.
- Query-builder `$in` / `$nin` (N3-I2): unchanged since R1.

None of the remaining findings sit on the V8↔Rust boundary in a way the runtime macros could fix — they are all structural Rust-side allocation choices in `exec.rs`, `broker.rs`, and `query.rs`. The score will tip past 80 once N3-C1 lands and either N3-I1 or N3-I2 follows; past 90 once all three plus R1 I3 are addressed.

No bench numbers cited because no plugin-db-touching bench results exist under `crates/runtime/benches/results-*.txt`. The runtime benches exercise fetch / WS / RPC dispatch, not the DB callback chain. Any quantitative claim would be a fabrication; a future plugin-db bench harness (e.g. `cargo bench --bench db_findone` with an in-memory backend) would let this review carry hard numbers.
