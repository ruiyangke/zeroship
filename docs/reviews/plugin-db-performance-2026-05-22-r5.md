# plugin-db Performance Review — 2026-05-22 r5

Commit: HEAD (`3e0656e6`). Cycle baseline: r4 (74 / 100, head `40765dc8`).
Mode: read-only, no benchmarks executed. The runtime bench suite under
`crates/runtime/benches/results-*.txt` still tops out at
`results-2026-05-10-after-eternal-sweep.txt`, and none of those files
exercise plugin-db paths (`grep -l "plugin-db\|findOne\|insertOne\|db_\|crud"
results-*.txt` → empty). No bench harness exists under
`crates/plugin-db/benches/`. Every quantitative claim below is therefore
annotated `unknown — needs measurement`.

---

## 1. Cycle perf-fix verification

### CLOSED — [I36] (5ceb6daa): `validate_collection` lowercase-alloc

`crates/plugin-db/src/query.rs:61-99` is the rewritten `validate_collection`.
Byte-comparison shape is in place:

```
77      // Reserved-prefix checks via byte-slice equality avoid an allocating
78      // .to_ascii_lowercase() per CRUD dispatch (performance r4 N4-I4).
79      let bytes = name.as_bytes();
80      if bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_") {
81          return Err(...);
82      }
83      ...
85      if bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship") {
```

Grep evidence — `to_ascii_lowercase` / `to_lowercase` no longer fires on
the CRUD hot path:

```
$ rg 'to_ascii_lowercase|to_lowercase' crates/plugin-db/src
crates/plugin-db/src/replication.rs:94      Ok(app_id.to_ascii_lowercase())     # replication setup, cold
crates/plugin-db/src/replication.rs:224     msg.to_lowercase().contains(...)    # error classification, cold
crates/plugin-db/src/wal_consumer.rs:683    let lc = s.to_ascii_lowercase();    # fatal-classifier, cold
crates/plugin-db/src/query.rs:78            comment only (the fix's docstring)
crates/plugin-db/src/query.rs:373           ON DELETE/UPDATE action parse, validate-only
crates/plugin-db/src/query.rs:3378          test-only
```

None of the remaining call sites run per CRUD call. The post-fix
prefix check is two `eq_ignore_ascii_case` calls against literal
3- and 10-byte slices — both are SIMD-friendly with no heap alloc.

Verification: `query.rs:61-99` verbatim + grep above. Bench: unknown —
needs measurement (no `cargo bench --bench db_*` harness exists; the
commit body says `cargo test -p zeroship-plugin-db --lib → 336 passed`
but offers no perf number).

### Other recent commits — perf impact verification

- `0e58c4e8` + `b32ba383` (broker two-level HashMap): re-verified at
  `broker.rs:402-468` and `:480-529`. Two-level `HashMap<String,
  HashMap<String, Vec<Subscription>>>`. `has_subscribers` (line 460)
  and `publish` (line 480) both look up by `&str` via the `Borrow<str>`
  impl on `String` keys → zero allocation on the hot path. Pruning at
  `:523-528` drops the per-app map when its last collection empties.
  No change vs. r4 — closed correctly.
- `37e61803` (update_backfill_progress before COMMIT): correctness fix.
  The UPDATE moved from after-COMMIT to before-COMMIT, plus an extra
  `rollback_and_return` on error. One extra possible rollback per
  failed batch — but the prior code stranded the connection on the
  error path, which is worse. Net: zero hot-path delta.
- `4cbe9fa1` (subscription V8-alloc ordering): correctness only. Same
  number of allocations on the success path, two fewer on the error
  path (no broker leak). No perf delta.
- `07205e54` (`pub` → `pub(crate)` demotes + comment fix): visibility
  only. No codegen difference expected. No perf delta.

---

## 2. Re-walk audit — fresh findings

### CRITICAL — none

No CRITICAL regressions vs. r4. The r4 cycle's two fixes (N3-C1 and
N3-I1) remain closed; the r5 cycle's [I36] is closed. The remaining
hot-path costs are structural or single-file IMPORTANTs.

### IMPORTANT

#### N5-I1 — `row_to_json` / `column_to_json` O(N²) per row (UNCHANGED since N4-I3)

**File:** `crates/plugin-db/src/v8_bridge.rs:353-361` + every
`row.try_get::<_, T>(name)` and `row.raw_value(name)` call at
`:369-495`.

Why: `column_to_json` accepts `name: &str` and resolves the column
index inside `row.try_get` / `row.raw_value`. The `RowIndex for str`
impl at `crates/compio-postgres/src/row.rs:65-82` is:

```rust
impl RowIndex for str {
    fn __idx<T>(&self, columns: &[T]) -> Option<usize>
    where T: AsName {
        if let Some(idx) = columns.iter().position(|d| d.as_name() == self) {
            return Some(idx);
        };
        columns.iter().position(|d| d.as_name().eq_ignore_ascii_case(self))
    }
}
```

Linear scan with a case-insensitive fallback pass. For each of N
columns in `row_to_json`'s loop we do an O(N) name lookup → O(N²) per
row. A 20-column row pays 400 string compares; a 100-row result set
pays 40 000 — fired on every `find()` / `findOne` / `insert RETURNING`.

The `RowIndex for usize` impl at `:49-61` is `O(1)`, but `column_to_json`
never reaches it.

Fix: take the column index by enumeration in the outer loop and pass
it down:

```rust
pub(crate) fn row_to_json(row: &compio_postgres::Row) -> Value {
    let mut obj = serde_json::Map::with_capacity(row.columns().len());
    for (idx, col) in row.columns().iter().enumerate() {
        let value = column_to_json_by_idx(row, idx, col.type_().oid());
        obj.insert(col.name().to_string(), value);
    }
    Value::Object(obj)
}
```

…and have `column_to_json_by_idx` use `row.try_get::<usize, T>(idx)` /
`row.raw_value::<usize>(idx)` everywhere. Single-file change inside
plugin-db (the index variant on `try_get` / `raw_value` already exists
in compio-postgres — it's the `RowIndex for usize` impl at `row.rs:49`).

Severity: this is the largest remaining structural hot-path cost on the
read path. Severity stays IMPORTANT (not CRITICAL) because the absolute
cost per column compare is small and the row decode still completes;
"largest" is relative to the now-much-smaller per-call alloc budget.

Verification: `v8_bridge.rs:353-361` + `:369-495` verbatim
(every match arm calls `row.try_get(name)` or `row.raw_value(name)`)
+ `compio-postgres/src/row.rs:65-82`. Bench: unknown — needs measurement
(would want a `db_find_wide_table` bench against a 30-column table).

#### N5-I2 — `$in` / `$nin` placeholder `Vec<String>` + `join` (UNCHANGED since R1)

**File:** `crates/plugin-db/src/query.rs:1944-1956` (`$in`),
`:1957-1969` (`$nin`).

```
1944  "$in" => {
1945      let arr = val.as_array().ok_or_else(|| { ... })?;
1948      let placeholders: Vec<String> = arr
1949          .iter()
1950          .map(|v| {
1951              params.push(value_to_param(v));
1952              format!("${}", params.len())
1953          })
1954          .collect();
1955      format!("{col} IN ({})", placeholders.join(", "))
1956  }
```

Why: N-element `$in` allocates N small `String`s (one per placeholder),
collects them into a `Vec<String>`, then `join(", ")` allocates a fresh
String and discards the per-element smalls. Same on `$nin`. Hot path:
every `find({ id: { $in: [...] } })` — common "load these rows by ID"
shape.

Fix: stream into one buffer (uses `std::fmt::Write`):

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

One allocation regardless of array length, eliminates the `placeholders`
Vec and the intermediate `.join(", ")`.

Verification: `query.rs:1944-1969` verbatim. No commit between r4 and r5
touches these lines. Bench: unknown — needs measurement.

#### N5-I3 — `migrations::exec_fetch_batch` serialises `Vec<Value>` → `String` for the V8 boundary (UNCHANGED since R1)

**File:** `crates/plugin-db/src/migrations.rs:432-434`

```
432  let rows = rows_result.map_err(|e| coded_db("migration fetch", ...))?;
433  let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
434  Ok(Value::Array(row_jsons).to_string())
```

Same shape as r4 N4-I2 (and r1 C1 before the main CRUD path was fixed,
but on the backfill side). Caller in `v8_classes/migration.rs:229` wraps
the String in `JsonValue` → `ResolveValue::Json` → `v8::json::parse`.
The intermediate String is wasted — the rows were just `Vec<Value>` and
will be re-parsed inside V8.

Severity stays IMPORTANT (not CRITICAL): backfill batches are bounded
(≤ 10 000 rows per `exec_fetch_batch`) and only run during deploy /
schema changes. CRUD hot path is unaffected.

Fix: change `exec_fetch_batch`'s return type to `Vec<Value>` (matching
`exec_query`), let the v8_class wrapper hand it through. Two-file
change (`migrations.rs` + `v8_classes/migration.rs`).

Verification: `migrations.rs:432-434` verbatim. Caller signature at
`v8_classes/migration.rs:219-231`. Bench: unknown — needs measurement.

### MINOR

#### N5-M1 — `wal_consumer::emit_for_tuple` clones `changed_columns` per published frame (UNCHANGED since N4-M1)

**File:** `crates/plugin-db/src/wal_consumer.rs:559-563`

```
559  let changed_columns: Vec<String> = rel
560      .columns
561      .iter()
562      .map(|c| c.name.clone())
563      .collect();
```

`rel.columns` is the cached `RelationEntry`. The column names are
stable for the relation's lifetime, but the Vec of clones is rebuilt
on every published frame (only fires past the `has_subscribers` gate
at `:548`).

Fix: cache `Vec<Arc<str>>` on `RelationEntry`; clone the `Arc<str>`
into the published event. Allocate once at Relation-message decode;
each WAL frame is a refcount bump.

Verification: `wal_consumer.rs:548-563`. Bench: unknown — needs measurement.

#### N5-M2 — `wal_consumer::emit_for_tuple` + `emit_local` clone `app_id` / `collection` per event (UNCHANGED since N4-M2)

**File:** `crates/plugin-db/src/wal_consumer.rs:568-572` (WAL path)
and `:219-227` (`emit_local`, the autocommit path).

```
568  publish(&ChangeEvent {
569      app_id: self.app_id.clone(),
570      collection: rel.table.clone(),
```

Both Strings are stable for the consumer's / collection's lifetime.
`broker::publish` (`broker.rs:504`) then `Rc::new(event.clone())`s the
whole event — including those Strings — once per publish total. The
clones at `:569-570` and `:220-221` are pure overhead before that.

Fix: switch `ChangeEvent.app_id` and `.collection` to `Arc<str>` (or
`Rc<str>` — broker is per-thread). Cache one `Arc<str>` per relation
in `RelationEntry`; cache `self.app_id` once on `WalConsumer`. The
`emit_local` autocommit path can take `Arc<str>` arguments instead of
`&str`.

Verification: `wal_consumer.rs:203-228` + `:559-577` + `broker.rs:504`.
Bench: unknown — needs measurement.

#### N5-M3 — `dispatch_aggregate` builds a fallback `Value::Object` unconditionally (UNCHANGED since N4-M3)

**File:** `crates/plugin-db/src/crud.rs:402-410`

```
402  {
403      let captured_filter = pipeline
404          .as_array()
405          .and_then(|stages| stages.first())
406          .and_then(|stage| stage.get("$match"))
407          .cloned()
408          .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
409      crate::read_set::record_if_active(collection, &captured_filter);
410  }
```

`read_set::record_if_active` early-returns at `read_set.rs:370` when
no capture is active (which is the common case — captures only fire
under `useQuery`). The unconditional `captured_filter` construction
at 403-408 (including `.cloned()` on a potentially-large `Value`) is
wasted work in that common case.

Fix: probe `read_set::is_active()` first:

```rust
if crate::read_set::is_active() {
    let captured_filter = pipeline
        .as_array()
        .and_then(|stages| stages.first())
        .and_then(|stage| stage.get("$match"))
        .cloned()
        .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    crate::read_set::record_if_active(collection, &captured_filter);
}
```

`read_set::is_active()` already exists at `read_set.rs:357-359` and
returns `bool` cheaply (one thread-local borrow). The `dispatch_find` /
`dispatch_find_one` / `dispatch_count` paths don't have this redundancy
— they pass `&filter` directly without cloning.

Verification: `crud.rs:402-410` + `read_set.rs:357-372`. Bench: unknown
— needs measurement.

#### N5-M4 — `dispatch_*` write paths allocate two `String`s to move into async (UNCHANGED since R3)

**File:** `crates/plugin-db/src/crud.rs:218-219, 246-247, 279-280,
308-309, 340-341, 368-369, 514-515, 553-554`

```
218  let coll = collection.to_string();
219  let app = app_id.to_string();
```

8 dispatch sites × 2 String allocs = 16 String allocs per workload of
1 each of the 8 mutating ops. Was r3 N3-M4 / r4 N4-M4. Fix: switch the
`Collection` v8-class's stored name / app to `Rc<str>` so `coll =
Rc::clone(&self.name)` is a refcount bump.

#### N5-M5 — `value_to_param` clones every `Value::String` (UNCHANGED since R1)

**File:** `crates/plugin-db/src/query.rs:2074`

```
2074  Value::String(s) => s.clone(),
```

One clone per query param. Was r3 N3-M3, r1 M1. Structural — fixing
requires threading a lifetime through `BuiltQuery`.

#### N5-M6 — `row_to_json` `serde_json::Map::new()` without capacity hint (UNCHANGED since R3)

**File:** `crates/plugin-db/src/v8_bridge.rs:354`

```
354  let mut obj = serde_json::Map::new();
```

`with_capacity(row.columns().len())` would skip rehashing on the 8-12
column case typical for a CRUD result. Was r3 N3-M5. Note: the N5-I1
fix sketch above already includes this (the rewrite passes the column
count to `with_capacity`).

#### N5-M7 — `build_aggregate` unconditionally allocates `agg_exprs` HashMap (UNCHANGED since R2)

**File:** `crates/plugin-db/src/query.rs:1480`. Was r3 N3-M2.

#### N5-M8 — `tuple_to_map` allocates `"NULL".to_string()` per NULL column (UNCHANGED since R2)

**File:** `crates/plugin-db/src/wal_consumer.rs:603`

```
603  TupleColumn::Null => {
604      out.insert(col.name.clone(), "NULL".to_string());
605  }
```

Was r3 N3-M1. Behind the `has_subscribers` gate so only fires on
subscribed tables. Fix: lift `"NULL"` to a `const &'static str` and
either (a) use `Cow<'static, str>` on the value side or (b) use a
sentinel that the broker recognises (cheaper, but contract change).

#### N5-M9 — `exec.rs::emit_for_rows` builds per-row tuple HashMap even on autocommit-with-subscribers (NEW, low)

**File:** `crates/plugin-db/src/exec.rs:206-249`

After the r4 cycle's `has_subscribers` gate (line 201-205), the
per-row work at 206-249 — `Vec<String>` of column names + `HashMap<String,
String>` of stringified values — fires once per affected row. For a
5-column UPDATE matching 100 rows that's ~12 small String allocs ×
100 = ~1200 allocs per CRUD call to `updateMany` with a wide filter.

This is structural-but-fixable: the broker only consumes the tuple
to feed predicate evaluation; the `Vec<String>` is the post-image
column list. The `HashMap` could be built lazily inside
`Subscription::accepts` from the original row Value (which is already
in scope at `emit_for_rows`'s caller). That would push the alloc cost
behind a second gate — "is there a subscriber whose predicate touches
this column?" — but requires restructuring the broker's accept path.

Severity: MINOR. Only fires on subscribed tables (the r4 closure
already filters out the no-subscriber case) and the per-row cost
scales with `affected_rows × columns`. For a typical reactive app
with one writer + many readers, this is a real but bounded cost.

Verification: `exec.rs:206-249`. Bench: unknown — needs measurement.

---

## 3. CRUD path full audit — dominant cost per call

Walked end-to-end for each operation post-[I36]. Allocations counted
only on the common (cache-hit, no-error) path.

### `findOne(filter)`

1. `record_if_active(collection, &filter)` — gated by `is_active()`,
   ~free when no `useQuery` capture is open.
2. `validate_collection` — **0 allocs on the prefix check post-[I36]**;
   one `as_bytes()` view + two `eq_ignore_ascii_case` calls. The
   `name.chars().all(...)` ASCII-alphanum check at `:90-97` is still
   a per-char loop but zero-alloc.
3. `quote_ident(app_id)` + `quote_ident(collection)` — 2 String allocs
   (structural; the quoted identifiers go into the SQL String).
4. `build_where(&filter, &mut params)` — for `{id: 1}`: 1 String alloc
   for the condition `"id" = $1` + 1 param push.
5. `build_find`'s `format!` — 1 String alloc for the SQL.
6. `setup_js_promise` — V8 resolver pair (structural).
7. `exec_query` → `param_refs: Vec<&str>` collect (1 Vec alloc) →
   `query_text_params` over the wire.
8. `rows_to_json_value` → `row_to_json` per row:
   - 1 `serde_json::Map::new()` (no capacity hint, **N5-M6**)
   - per column: 1 `col.name().to_string()` + O(N) name lookup
     (**N5-I1** — O(N²) total per row)
9. `first_row_or_null`: 1 String alloc (the JSON payload).
10. V8: `JSON.parse` on the payload — one parse.

Dominant cost on the read path: **N5-I1** (O(N²) column lookup) for
wide rows; otherwise the JSON `to_string` → `JSON.parse` round-trip
at steps 9-10 (structural — V8 boundary).

Per-call alloc count (4-col row, simple filter, no read-set capture):
~7 Strings + 1 Map + 1 Vec, plus 16 string compares for the column
lookups (4 columns × 4-position scan each). Sensible budget; the
remaining wins are N5-I1 and N5-M6.

### `insertOne(doc)` (no subscribers)

1. `validate_collection` — 0 prefix-check allocs post-[I36].
2. `quote_ident` × 2.
3. `build_insert`: per field — 1 `quote_ident`, 1 `value_to_param`
   (N5-M5 clones every `Value::String`), 1 placeholder `format!("$N")`.
4. SQL `format!` + `columns.join(", ")` + `placeholders.join(", ")` —
   3 String allocs.
5. **N5-M4** — 2 String allocs for `coll` + `app` to move into async.
6. `exec_mutation_with_emit` → `exec_mutation` → `run_sql`.
7. `rows_to_json_value` on the RETURNING row (1 row × column-count
   lookups → **N5-I1**).
8. `emit_for_rows`: **gate fires** (`!has_subscribers` → return). The
   per-row tuple+columns work is skipped. CLOSED (r4 closure).
9. `first_row_or_null` → V8 parse.

Dominant cost when no subscribers: same shape as `findOne`'s row
decode + the N5-M4 dispatch-closure allocations.

### `updateMany(filter, update)` (subscribers present)

Through step 7 same as insert. Then:

8. `emit_for_rows`: gate at `exec.rs:201-205` passes. For each affected
   row, builds `(Vec<String>, HashMap<String, String>)` — **N5-M9**.
   For a 5-column row: ~12 small String allocs per row. For 100 affected
   rows, ~1200 small String allocs total.
9. `queue_or_emit` → autocommit branch → `wal_consumer::emit_local`
   builds `ChangeEvent` with `app_id.to_string()` + `collection.to_string()`
   — **N5-M2**.
10. `broker::publish`: `Rc::new(event.clone())` — one event clone total,
    shared across all live subscribers.
11. For each subscriber: `accepts(&shared)` (O(read-set entries),
    short-circuits) + `push(SubscriptionMessage::Change(Rc::clone(&shared)))`
    — refcount bump.

Dominant cost when subscribed: **N5-M9** (per-row tuple HashMap build).
The broker fanout itself is bounded (see §5).

Verification: trace at `crud.rs:208-232` → `exec.rs:136-249` →
`wal_consumer.rs:203-228` → `broker.rs:480-529`. Bench: unknown — needs
measurement.

---

## 4. Schema-installation cost (registerModel)

**File:** `crates/plugin-db/src/orchestrator/register_model/mod.rs:63-101`
+ `crates/plugin-db/src/lib.rs:118-125` + `crates/plugin-db/src/context.rs:239-248`.

Fast path: `is_model_registered(app_id, collection)` at line 73 →
`context.is_model_registered`:

```
context.rs:239
pub fn is_model_registered(&self, app_id: &str, collection: &str) -> bool {
    let key = format!("{app_id}:{collection}");          // 1 String alloc
    self.registered_models.contains(&key)
}
```

The `format!` alloc per call is unchanged since r4 (called out as
"cold-path acceptable" in r4 §4). `registerModel` is called once per
`Collection` instance at app boot, not per CRUD request — so the
alloc-per-call is acceptable. The lock_guard refactor (`cbd12944`,
mentioned in the recent-commits header) gates DDL apply concurrency
via the OrchestratorLockGuard RAII; the lock acquisition is a
Postgres advisory-lock round-trip, dwarfing any Rust-side alloc cost.

No hot-path concern. Latency on the first call is dominated by
Postgres round-trips (introspect → diff → DDL apply → audit-row
writes), not by allocations.

Fix (low priority): switch `RegisteredModels` to a
`HashSet<(Arc<str>, Arc<str>)>` keyed by tuple, and pre-cache the
two `Arc<str>` on the `Collection` v8-class. The same `Arc<str>`s
can power the N5-M4 fix (`coll` / `app` for the async closures).
Two birds, one stone — but neither is on the hot path so the win
is small.

---

## 5. Broker fanout

`Broker::publish` (`broker.rs:480-529`) — re-verified at HEAD:

1. Two-level lookup: O(1) on `app_id`, O(1) on `collection`. Both
   borrow-based (`Borrow<str>` impl on `String` keys) so no alloc.
2. `subs.retain(|s| !s.is_closed())` — O(subscribers_on_this_collection);
   GCs closed entries in-place.
3. `Rc::new(event.clone())` — once per publish, regardless of fan-out.
   This is the structural cost: clones the `ChangeEvent` including the
   `new_tuple: HashMap<String, String>`.
4. Loop `for s in subs.iter()`: per subscriber, `s.accepts(&shared)` is
   O(read-set entries) with early-out on first matching predicate;
   `s.push(...)` is one `VecDeque::push_back` + one `Rc::clone`
   (refcount bump only — the SubscriptionMessage holds `Rc<ChangeEvent>`).
5. Bucket pruning at `:523-528`: drop the collection bucket if empty
   after the GC pass; drop the per-app inner HashMap if its last
   collection emptied. Keeps `has_subscribers` cheap on apps that
   churn ephemeral collections.

Total cost: O(subscribers_on_this_collection × predicate_entries).
Bounded by subscriber count and per-subscription read-set size — both
user-controlled, no platform-side fan-out pathology.

The only avoidable cost on this path is the `event.clone()` at line
504 (clones the HashMap). That cost is structural unless the broker
contract changes to hand subscribers a view rather than an owned event
— a contract change that needs to consider the fact that
`Subscription` queues live inside per-subscriber `VecDeque<SubscriptionMessage>`
and outlast the publish call.

Verdict: bounded, well-behaved. No regression vs. r4. Test at
`broker.rs:1268-1310` (`publish_drops_per_app_map_when_last_collection_empties`)
still locks the pruning contract.

---

## 6. WAL consumer hot path

`WalConsumer::dispatch` (`wal_consumer.rs:456-504`) and
`emit_for_tuple` (`:518-577`):

Per pgoutput Insert / Update / Delete:

1. `relations.get(&rel_id)` — O(1) HashMap lookup.
2. `rel.namespace != self.app_id` — String compare (no alloc).
3. **`has_subscribers(&self.app_id, &rel.table)` gate at line 548**
   — the r3/r4 closure. When the collection has no subscribers, return
   immediately. No allocations from this point on.
4. When subscribed:
   - `primary_key_index()` — O(columns) linear scan (no alloc).
   - **N5-M1** — Vec<String> of column-name clones (one Vec alloc +
     N String clones).
   - **N5-M8** — `tuple_to_map` builds two HashMaps (new_tuple,
     and old_tuple for UPDATE). Each entry: 1 column-name clone + 1
     value clone or `"NULL".to_string()` per NULL column.
   - **N5-M2** — `publish(&ChangeEvent { app_id: self.app_id.clone(),
     collection: rel.table.clone(), ... })`.
5. Broker `publish` (see §5) — bounded.

Allocation count per subscribed WAL frame (5-col row, no UPDATE old_tuple):

- 1 Vec<String> alloc + 5 String clones (changed_columns, **N5-M1**)
- 1 HashMap + 5 String clones (key) + 5 String clones (value) (new_tuple)
- 2 String allocs (app_id + collection, **N5-M2**)
- 1 Rc + 1 HashMap clone inside `broker::publish::event.clone()`

≈ 20 allocs per WAL frame on a subscribed table. Caps cleanly with
column count; with `Arc<str>` interning on column names + per-relation
cached `app_id` / `collection`, this could drop to ~5 allocs.

Verification: trace at `wal_consumer.rs:456-577` + `broker.rs:480-529`.
Bench: unknown — needs measurement.

---

## 7. Top-3 next-priority items

1. **N5-I1** (`row_to_json` O(N²) column lookup): single-file change
   inside `v8_bridge.rs` — switch the inner decode to index-based.
   The compio-postgres-side `RowIndex for usize` impl is already O(1).
   Runs on every read result decode. Largest remaining structural
   hot-path cost.
2. **N5-I2** (`$in` / `$nin` placeholder Vec): trivial single-spot fix
   in `query.rs`, no API impact. Hot for any "load these IDs" filter.
3. **N5-M9** (per-row tuple HashMap build in `emit_for_rows`): only
   fires on subscribed tables, but scales with `affected_rows × columns`.
   Higher engineering cost than (1) or (2) — touches the broker accept
   contract — but the largest remaining cost when subscribers are
   active.

After (1)+(2): the per-call alloc budget on the read path is dominated
by N5-M6 (`Map::with_capacity`, trivial), N5-M5 (`Value::String`
param clone, structural), and the unavoidable V8 JSON boundary.

---

## 8. Score

**76 / 100** (vs. r4's 74, r3's 70, r2's 57, r1's 44).

Movement vs r4:
- **+2** for the [I36] closure. Drops one String alloc per CRUD
  dispatch — small absolute win, runs on every call. Verified at
  `query.rs:61-99` + grep showing `to_ascii_lowercase` no longer
  fires on the CRUD path. Bench: unknown — needs measurement.
- **+0** for the broker two-level HashMap closure (already credited in
  r4's score; the post-cycle commits `0e58c4e8` + `b32ba383` were
  baked in to the r4 baseline).
- **+0** for `37e61803` / `4cbe9fa1` / `07205e54` — correctness / API
  visibility / docs only.
- **−0** for newly found items: N5-M9 (per-row tuple HashMap in
  `emit_for_rows`) is real but pre-dates the cycle — it was implicit
  in the r4 trace at "structural cost when subscribers exist", just
  not separately scored.

What's still pulling the score below 80:
- **N5-I1** — O(N²) column lookup on every read decode. Bench: unknown
  — needs measurement.
- **N5-I2** — `$in` placeholder Vec allocation. Bench: unknown — needs
  measurement.
- **N5-I3** — migrations backfill String round-trip. Bench: unknown —
  needs measurement.
- The persistent absence of a `cargo bench --bench db_*` harness —
  every round writes "unknown — needs measurement" against findings
  that are static-analysis counts. A future round with one bench
  landed would convert several of these "important-looking but
  unmeasured" claims into hard numbers (or refute them).

What would lift past 85:
- N5-I1 fixed (index-based row decode): closes the dominant per-call
  cost on the read path.
- N5-I2 fixed: trivial.
- One DB-touching bench harness landed under
  `crates/plugin-db/benches/` (e.g. `db_findone.rs`, `db_insert.rs`,
  `db_find_wide.rs`): future rounds carry numbers, not assertions.

What would lift past 90:
- All three IMPORTANTs (N5-I1, N5-I2, N5-I3) closed.
- The N5-M2/M4 path (Arc<str> for app_id / collection across CRUD +
  WAL + broker) refactored. Cuts per-event allocation by ~3-4 Strings.
- A documented latency-vs-throughput baseline (μs / op) for the
  CRUD operations, so future cycles can detect regressions.

Bench reminder: no plugin-db bench results exist in
`crates/runtime/benches/results-*.txt` (latest is
`2026-05-10-after-eternal-sweep.txt`; everything post-2026-04 exercises
fetch/WS/RPC, not the DB path) and no harness exists under
`crates/plugin-db/benches/`. Every "saves N allocs" in this report is
a static-analysis count, not a measurement. Score and severity are
qualitative; the next cycle could resolve "is N5-I1 a 5% find or a
50% find on wide rows?" with a single measurement.
