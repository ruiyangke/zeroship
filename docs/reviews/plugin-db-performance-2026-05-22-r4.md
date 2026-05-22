# plugin-db Performance Review — 2026-05-22 r4

Commit: HEAD (`40765dc8`). Cycle baseline: r3 (70/100).
Mode: read-only, no benches run. The runtime bench suite under
`crates/runtime/benches/results-*.txt` does not exercise plugin-db
paths; no DB-touching numbers exist post-`results-2026-05-10-after-eternal-sweep.txt`,
so every quantitative claim below is annotated `unknown — needs measurement`.

---

## 1. Cycle perf-fix verification

### CLOSED — N3-C1 (49b0b98e): `exec_mutation_with_emit` gate before tuple build

`crates/plugin-db/src/exec.rs:189-205` is the rewritten `emit_for_rows`. The order is:

```
189  fn emit_for_rows(rows, app_id, collection, op) {
195      if rows.is_empty() { return; }
201      if is_app_suppressed(app_id) || !has_subscribers(app_id, collection) {
204          return;
205      }
206      for row in rows {                                  // tuple build starts here
207          let pk = ...
218          let (columns, tuple): (Vec<String>, HashMap<String, String>) = ...
245      }
```

The gate at lines 201-205 fires BEFORE the per-row build loop at 206-249. Both legs of the disjunction (`is_app_suppressed || !has_subscribers`) short-circuit on first true. The `exec_mutation` call upstream still runs the SQL (correct — the write must execute regardless of subscribers); only the broker-event materialisation is skipped.

`#[cfg(test)] tests::record_tuple_built()` at line 247 is the recorder unit-tests use to assert the gate. The 3 tests added in 49b0b98e (`exec.rs::tests::tuple_build_skipped_when_app_suppressed`, `..._when_no_subscriber`, `..._with_active_subscriber`) verify the gate independent of a live Postgres pool.

Verification: file `exec.rs` lines 189-249. Bench: unknown — needs measurement (no DB bench harness exists).

### CLOSED — N3-I1 (0e58c4e8): broker two-level HashMap, alloc-free `has_subscribers`

`crates/plugin-db/src/broker.rs:408` is `by_key: HashMap<String, HashMap<String, Vec<Subscription>>>` (was `HashMap<(String, String), Vec<_>>`).

Hot-path lookups (`broker.rs:460-468` and `:485-491`):

```
460  pub fn has_subscribers(&self, app_id: &str, collection: &str) -> bool {
461      let Some(by_collection) = self.by_key.get(app_id) else { return false; };
464      let Some(subs) = by_collection.get(collection) else { return false; };
467      !subs.is_empty()
468  }
485  let Some(by_collection) = self.by_key.get_mut(event.app_id.as_str()) else { return; };
488  let Some(subs) = by_collection.get_mut(event.collection.as_str()) else { return; };
```

`HashMap<String, _>::get(&str)` uses the `Borrow<str>` impl on `String` keys → zero String allocs on the lookup. Same for the inner `HashMap<String, Vec<_>>`. Tests at `broker.rs:1268-1310` (`has_subscribers_takes_str_no_string_alloc_at_call_site`, `publish_drops_per_app_map_when_last_collection_empties`) lock the contract.

Owning-alloc path is now isolated to `subscribe` (`broker.rs:424-443`): `entry(app_id.to_string()).or_default().entry(collection.to_string()).or_default().push(sub.clone())`. Two `String::from` per subscribe; subscribe is the cold path (once per `db.subscribe(...)` JS call). The per-app inner `HashMap` is dropped when its last collection empties (`broker.rs:523-528`), so the publish lookup stays cheap on apps that churn ephemeral collections.

Verification: `broker.rs` lines 402-468 + 485-528 + test at 1212-1310. Bench: unknown — needs measurement.

---

## 2. Re-walk audit — fresh findings

### CRITICAL — none

No CRITICAL regressions against the r3 baseline. The two perf fixes from the cycle are correctly placed and the build still compiles. Both r3-closures stay closed.

### IMPORTANT

#### N4-I1 — `$in` / `$nin` placeholder `Vec<String>` + `join` (UNCHANGED since R1)

**File:** `crates/plugin-db/src/query.rs:1946-1953` (`$in`), `:1959-1966` (`$nin`)

```
1946  let placeholders: Vec<String> = arr
1947      .iter()
1948      .map(|v| {
1949          params.push(value_to_param(v));
1950          format!("${}", params.len())
1951      })
1952      .collect();
1953  format!("{col} IN ({})", placeholders.join(", "))
```

Why: for an N-element `$in`, this allocates N small `String`s (`format!("$N")`), pushes them into a `Vec<String>` (1 Vec alloc + N moves), then `join(", ")` allocates a fresh `String` and frees the smalls. Same on `$nin`. Hot path: every `find({id: {$in: [...]}})` — a common shape for "load these specific IDs" queries.

Fix: stream into one `String` buffer:

```rust
use std::fmt::Write as _;
let mut sql = String::with_capacity(col.len() + 6 + arr.len() * 5);
let _ = write!(sql, "{col} IN (");
for (i, v) in arr.iter().enumerate() {
    params.push(value_to_param(v));
    if i > 0 { sql.push_str(", "); }
    let _ = write!(sql, "${}", params.len());
}
sql.push(')');
```

One allocation regardless of array length. Saves N+1 String allocs per `$in`/`$nin` clause.

Verification: `query.rs:1946-1953`, `:1959-1966` verbatim. No commit between r3 and r4 touches these lines. Bench: unknown — needs measurement.

#### N4-I2 — `migrations::exec_fetch_batch` serialises `Vec<Value>` → `String` for the V8 boundary (UNCHANGED since R1)

**File:** `crates/plugin-db/src/migrations.rs:423-425`

```
423  let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
424  let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
425  Ok(Value::Array(row_jsons).to_string())
```

Why: same shape as R1 C1 — but on the backfill path. Caller (`v8_classes/migration.rs:229` → `JsonValue` newtype) hands the String through `ResolveValue::Json` → `v8::json::parse`. The intermediate `String` is a wasted alloc the size of the encoded result set.

Severity stays IMPORTANT (not CRITICAL): backfill batches are bounded (≤ 10 000 rows per `exec_fetch_batch`) and only run during deploys. CRUD hot path is unaffected.

Fix: change `exec_fetch_batch`'s return type to `Vec<Value>` (matching `exec_query`), let the v8_class wrapper handle the boundary serialisation. Two-file change (migrations.rs + v8_classes/migration.rs).

Verification: `migrations.rs:425` verbatim. Caller signature confirmed at `v8_classes/migration.rs:219-231` (`async fn fetch_batch -> Result<JsonValue, OpError>` wraps the string). Bench: unknown — needs measurement.

#### N4-I3 — `row_to_json` and `column_to_json` are O(N²) per row in column count (NEW)

**File:** `crates/plugin-db/src/v8_bridge.rs:353-361`, plus every `row.try_get::<_, T>(name)` and `row.raw_value(name)` call below.

```
353  pub(crate) fn row_to_json(row: &compio_postgres::Row) -> Value {
354      let mut obj = serde_json::Map::new();
355      for col in row.columns() {
356          let key = col.name().to_string();
357          let value = column_to_json(row, col.name(), col.type_().oid());
358          obj.insert(key, value);
359      }
360      Value::Object(obj)
361  }
```

`column_to_json` then calls `row.try_get::<_, T>(name)` with `name: &str`. The lookup at `compio-postgres/src/row.rs:65-82` is `columns.iter().position(|d| d.as_name() == self)` — a linear scan with a case-insensitive fallback. So for each of N columns we do an O(N) name lookup → O(N²) per row.

For a 20-column row that's 400 string compares; for a 100-row result set, 40 000. Hot path: every `find()` result decode.

Why this is structural-but-fixable: `row.columns()` already gives a `&[Column]` with index. The lookup-by-name only happens because `column_to_json` accepts `name: &str` and re-resolves. Switch the inner loop to iterate by index:

```rust
for (idx, col) in row.columns().iter().enumerate() {
    let value = column_to_json_by_idx(row, idx, col.type_().oid());
    obj.insert(col.name().to_string(), value);
}
```

…and add a `row.try_get::<usize, T>(idx)` path (the `RowIndex for usize` impl at `row.rs:49-61` is `O(1)`). Saves O(N×col_count²) string compares per row.

Same applies to `raw_value(name)` at `v8_bridge.rs:412`: `raw_value(name)` resolves the index via the `RowIndex` trait; a `raw_value(idx)` form already exists (the `RowIndex for usize` impl). Pre-resolve once.

Verification: `v8_bridge.rs:353-361` + `column_to_json` body + `compio-postgres/src/row.rs:65-82` for the O(N) lookup. Bench: unknown — needs measurement (a `cargo bench --bench db_find_wide_table` doesn't exist).

#### N4-I4 — `validate_collection` allocates a fresh lowercase String per CRUD call (NEW)

**File:** `crates/plugin-db/src/query.rs:77-87`

```
77  let lower = name.to_ascii_lowercase();
78  if lower.starts_with("pg_") {
79      return Err(...);
80  }
83  if lower.starts_with("__zeroship") {
```

`validate_collection` runs at the top of every `build_find` / `build_insert` / `build_update_one` etc. — every CRUD call. The `name.to_ascii_lowercase()` clone is pure overhead just to test two short prefixes.

Fix: byte-level case-insensitive compare:

```rust
let bytes = name.as_bytes();
if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_") {
    return Err(...);
}
if bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship") {
    return Err(...);
}
```

Zero allocation. Saves one String alloc per CRUD call (N for N requests-per-second; on a 100K-RPS worker that's 100K avoidable allocs/sec — but each alloc is small).

Verification: `query.rs:77` literal `name.to_ascii_lowercase()`. Bench: unknown — needs measurement.

### MINOR

#### N4-M1 — `wal_consumer::emit_for_tuple` clones `changed_columns` per published frame

**File:** `crates/plugin-db/src/wal_consumer.rs:559-563`

```
559  let changed_columns: Vec<String> = rel
560      .columns
561      .iter()
562      .map(|c| c.name.clone())
563      .collect();
```

`rel.columns` is the cached RelationEntry. Its column names are stable for the relation's lifetime — but we clone them into a fresh `Vec<String>` on every Insert/Update/Delete that passes the `has_subscribers` gate (after the r3 gate fired). The cloned vec then gets cloned again inside `ChangeEvent` via `Rc::new(event.clone())` at `broker.rs:504`.

Why MINOR: only runs when subscribers exist (gate at line 548 closes the no-subscriber case). The cost scales with column count × frames-per-sec only on subscribed tables.

Fix: cache a pre-built `Vec<String>` (or `Vec<Arc<str>>`) on `RelationEntry`. Allocate once at Relation-message decode time; clone the `Arc<str>` refcounts on each event. Saves O(columns) String clones per WAL frame on subscribed tables.

Verification: `wal_consumer.rs:559-563` verbatim, called from `emit_for_tuple`. Bench: unknown — needs measurement.

#### N4-M2 — `wal_consumer::emit_for_tuple` clones `rel.table` and `self.app_id` per event (UNCHANGED)

**File:** `crates/plugin-db/src/wal_consumer.rs:568-572`

```
568  publish(&ChangeEvent {
569      app_id: self.app_id.clone(),
570      collection: rel.table.clone(),
571      ...
572  });
```

Both Strings are stable for the consumer's lifetime. The `ChangeEvent.app_id: String / collection: String` are then cloned a second time by `broker::publish`'s `Rc::new(event.clone())`.

Fix: switch `ChangeEvent.app_id` / `.collection` to `Arc<str>` (or `Rc<str>` — broker is per-thread). Cache one `Arc<str>` per relation in the relation cache; cache `self.app_id` once. Allocates twice at consumer start; clone is refcount bump per event.

Verification: `wal_consumer.rs:568-572` + `broker.rs:504` `Rc::new(event.clone())`. Bench: unknown — needs measurement.

#### N4-M3 — `dispatch_aggregate` builds a fallback `Value::Object(Map::new())` unconditionally (NEW)

**File:** `crates/plugin-db/src/crud.rs:402-410`

```
402  {
403      let captured_filter = pipeline
404          .as_array()
405          .and_then(|stages| stages.first())
406          .and_then(|stage| stage.get("$match"))
407          .cloned()                                       // alloc when $match present
408          .unwrap_or_else(|| Value::Object(serde_json::Map::new()));   // alloc otherwise
409      crate::read_set::record_if_active(collection, &captured_filter);
410  }
```

`read_set::record_if_active` early-returns when no capture is active (`read_set.rs:370`). The unconditional `captured_filter` construction at 403-408 (including the `.cloned()` on a `Value`) is wasted work in the common case (no `useQuery` capture).

Fix: check `read_set::is_active()` first; only construct the filter when it would be recorded.

```rust
if crate::read_set::is_active() {
    let captured_filter = ...;
    crate::read_set::record_if_active(collection, &captured_filter);
}
```

Verification: `crud.rs:402-410`, `read_set.rs:357-372`. Bench: unknown — needs measurement.

#### N4-M4 — `dispatch_*` write paths allocate two extra `String`s to move `app_id`/`collection` into async (UNCHANGED since R3)

**File:** `crates/plugin-db/src/crud.rs:218-219, 247-248, 280-281, 308-309, 340-341, 368-369, 515-516, 552-553`

`let coll = collection.to_string(); let app = app_id.to_string();` — 2 String allocs per write op, 8 dispatch sites total. Was r3 N3-M4. Fix (same as r3): switch `Collection`'s stored name/app to `Rc<str>`; `coll = Rc::clone(&self.name)` becomes a refcount bump.

#### N4-M5 — `value_to_param` clones every `Value::String` (UNCHANGED since R1)

**File:** `crates/plugin-db/src/query.rs:2074`

```
2074  Value::String(s) => s.clone(),
```

One clone per query param. Was r3 N3-M3, r1 M1. Structural — requires lifetime threading through `BuiltQuery`.

#### N4-M6 — `row_to_json` `serde_json::Map::new()` without capacity hint (UNCHANGED since R3)

**File:** `crates/plugin-db/src/v8_bridge.rs:354`

`with_capacity(row.columns().len())` would skip rehashing for 8-12 column rows. Was r3 N3-M5.

#### N4-M7 — `build_aggregate` unconditionally allocates `agg_exprs` HashMap (UNCHANGED since R2)

**File:** `crates/plugin-db/src/query.rs:1480`. Was r3 N3-M2.

#### N4-M8 — `tuple_to_map` allocates `"NULL".to_string()` per NULL column (UNCHANGED since R2)

**File:** `crates/plugin-db/src/wal_consumer.rs:603`. Was r3 N3-M1. Behind the r3 `has_subscribers` gate so only fires on subscribed tables.

---

## 3. CRUD path full audit — dominant cost per call

Walked end-to-end for each operation. Allocations counted only when they fire on the common (cache-hit, no-error) path.

### `findOne(filter)`

Per-call cost, in order:

1. `record_if_active(collection, &filter)` — gated, ~free when no `useQuery` captures.
2. `validate_collection` / `validate_schema` — **1 String alloc (`to_ascii_lowercase`, N4-I4)**, ~free otherwise.
3. `quote_ident(app_id)` + `quote_ident(collection)` — 2 String allocs (the quoted-form strings); structural.
4. `build_where(filter, &mut params)` — recursive; for a simple `{id: 1}`: 1 String alloc for the condition `"id" = $1` + 1 param push (`value_to_param`).
5. `build_find`'s `format!` for SQL — 1 String alloc.
6. `setup_js_promise` — V8 promise/resolver pair; structural.
7. `exec_query`: `param_refs: Vec<&str>` collect (1 Vec alloc), then `run_sql` → `query_text_params` over the wire.
8. `rows_to_json_value` → `row_to_json`:
   - per row: 1 `serde_json::Map::new()` (no capacity hint, **N4-M6**)
   - per column: 1 `col.name().to_string()` (~M6 again) + O(N) `try_get(name)` lookup (**N4-I3** — O(N²) per row)
9. `first_row_or_null`: `rows.into_iter().next().unwrap_or(Null).to_string()` — 1 String alloc (the JSON payload).
10. V8: `JSON.parse` on the payload — one parse.

Dominant cost: the O(N²) column-name lookup at step 8 for wide rows (N4-I3); otherwise the JSON `to_string` + `JSON.parse` round trip at steps 9-10, which is structural.

### `insertOne(doc)` (no subscribers)

1. validate_collection — **1 String alloc (N4-I4)**.
2. `quote_ident` × 2.
3. `build_insert`: iterates `doc` object once; per field: 1 `quote_ident`, 1 `value_to_param`, 1 placeholder `format!("$N")`.
4. `format!` SQL — 1 String alloc + `columns.join(", ")` + `placeholders.join(", ")` (2 more allocs).
5. `dispatch_insert` extra: **2 String allocs (N4-M4)** for `coll` + `app`.
6. exec_mutation → run_sql.
7. `rows_to_json_value` over the RETURNING row.
8. `emit_for_rows`: **gate fires** (`!has_subscribers` → return). Saves the per-row tuple+columns work. CLOSED.
9. `first_row_or_null` → V8 parse.

With the r3 N3-C1 fix, the dominant cost is now the same as `findOne`'s row-decode + the M4 dispatch-closure allocations. No serde round-trip on the broker side.

### `update(filter, update)` (subscribers present)

Same as insert through step 7. Then:

8. `emit_for_rows`: gate passes → for each affected row, builds `(Vec<String> cols, HashMap<String,String> tuple)`. For a 5-column row: ~12 small String allocs (5 col-name clones + 5 key clones in the map + 5 value strings + the Vec + the HashMap). This is the structural cost when subscribers exist — fundamentally unavoidable without changing the broker contract.
9. `wal_consumer::emit_local` builds the `ChangeEvent` (`app_id.to_string()` + `collection.to_string()`, **N4-M2**).
10. `broker::publish`: `Rc::new(event.clone())` — clones the whole event including the HashMap.

Verification: trace at `crud.rs:208-232` → `exec.rs:136-205` → `wal_consumer.rs:203-228` → `broker.rs:480-529`. Bench: unknown — needs measurement.

---

## 4. Schema-installation cost (registerModel)

**File:** `crates/plugin-db/src/orchestrator/register_model/mod.rs:63-101`

Fast path: `is_model_registered(app_id, collection)` at line 73 → `context::with(|c| c.is_model_registered(app_id, collection))`. That call:

```
context.rs:239
pub fn is_model_registered(&self, app_id: &str, collection: &str) -> bool {
    let key = format!("{app_id}:{collection}");          // <-- 1 String alloc
    self.registered_models.contains(&key)
}
```

`HashSet<String>::contains(&key: &String)` — but allocates a fresh `format!`'d key each call. **N4-Cold**: registerModel is a cold path (once per Collection at boot), so the alloc per call is acceptable. Worth noting for completeness; not a hot-path concern.

Cold-path heavy lifting (introspect → diff → validate → apply DDL → audit-row writes) is dominated by Postgres round-trip latency, not Rust allocations. The advisory lock (`bootstrap.rs:98`) gates apply concurrency. No allocation hot-spots stood out on this re-walk.

---

## 5. Broker fanout — bounded?

`Broker::publish` (`broker.rs:480-529`):

1. Two-level lookup: O(1) on `app_id` + O(1) on `collection`.
2. `subs.retain(|s| !s.is_closed())` — O(subscribers_on_this_collection); GCs closed entries in-place.
3. `Rc::new(event.clone())` — once per publish, regardless of fan-out.
4. Loop `for s in subs.iter()`: per subscriber, `s.accepts(&shared)` is O(read-set entries) with early-out on first matching entry; `s.push(...)` is one `VecDeque::push_back` + 1 `Rc::clone`.

Total cost: O(subscribers_on_this_collection × predicate_entries). Bounded by subscriber count and read-set size, both user-controlled. Closed subscribers are pruned in the same pass so the bucket stays bounded.

Pruning at lines 523-528 drops the bucket when empty AND drops the per-app inner HashMap when its last collection empties. This keeps `has_subscribers` cheap on apps that churn ephemeral collections — verified by test `publish_drops_per_app_map_when_last_collection_empties` (`broker.rs:1299-1310`).

Verdict: bounded, well-behaved. No fan-out pathology. The structural cost is `event.clone()` at line 504 (clones the `HashMap` `new_tuple`) — but that's one clone per publish total, not per subscriber. Subscribers share the `Rc<ChangeEvent>`.

---

## 6. Top-3 next-priority items

1. **N4-I3** (`row_to_json` O(N²) column lookup): single-file change in `v8_bridge.rs`, no API contract impact. Runs on every read result. Largest remaining structural hot-path cost.
2. **N4-I1** (`$in` / `$nin` placeholder build): single-spot in `query.rs`, no API impact. Hot for any "load these IDs" filter.
3. **N4-I4** (`validate_collection` lowercase alloc): trivial fix, runs on every CRUD call. Smallest win individually but covers all paths.

After all three: the per-call allocation budget is dominated by `serde_json::Map::new()` (N4-M6, trivial capacity hint) and the unavoidable JSON `to_string` → `JSON.parse` round-trip at the V8 boundary.

---

## 7. Score

**74 / 100** (vs. r3's 70/100, r2's 57, r1's 44).

Movement vs r3:
- +4 for cycle closures: r3 CRITICAL (N3-C1) and r3 IMPORTANT (N3-I1) both shipped with unit-test coverage. No CRITICAL outstanding.
- +0 for r3's still-open items (N3-I2/N3-I3 carry forward as N4-I1/N4-I2 — same severity, same finding).
- −0 for newly found items: N4-I3 (O(N²) row decode) and N4-I4 (validate alloc) are real but neither is a regression against r3 — both pre-date the cycle and weren't called out then.

What pulls the score below 80:
- N4-I3 (O(N²) column lookup in `row_to_json`): runs on every read result. Wide tables pay quadratic. Bench: unknown — needs measurement.
- N4-I1 (`$in` / `$nin` placeholder Vec): unchanged since r1. Bench: unknown — needs measurement.
- N4-I2 (migrations backfill String round-trip): unchanged since r2. Bench: unknown — needs measurement.

What would lift past 85:
- N4-I3 fixed (move to index-based row decode): closes the dominant per-call cost on the read path.
- N4-I1 fixed: trivial, no API impact.
- One DB-touching bench harness landed (`cargo bench --bench db_findone`/`db_insert`/`db_find_wide`): every future round can carry quoted numbers instead of saying "unknown".

The cycle's two perf fixes are correctly placed with unit-test coverage. The remaining hot-path costs are now structural (V8 JSON boundary, serde_json `Value` round-trip) or call out specific allocations the next cycle can pick up cheaply. No CRITICAL outstanding; the IMPORTANTs are all single-file edits.

Bench reminder: no plugin-db bench results exist in `crates/runtime/benches/results-*.txt` (latest is `2026-05-10`; all post-`2026-04` files exercise fetch/WS/RPC). Quantitative claims would be fabrications — every "saves N allocs" in this report is a static-analysis count, not a measurement. A future `cargo bench --bench db_*` harness would let reviews carry hard numbers.
