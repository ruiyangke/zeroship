# plugin-db Performance Review — 2026-05-22 r6

Commit: HEAD (`51ced4a0`). Cycle baseline: r5 (76 / 100, head `3e0656e6`).
Mode: read-only, no benchmarks executed. No new bench result files appeared
under `crates/runtime/benches/results-*.txt` since r5 (`grep -l
"plugin-db\|findOne\|insertOne\|db_\|registerModel" crates/runtime/benches/`
→ empty); no harness under `crates/plugin-db/benches/` (directory still
absent). Every quantitative claim below remains a static-analysis count
annotated `unknown — needs measurement`.

Commits considered since r5 baseline (`git log 3e0656e6..HEAD --
crates/plugin-db/`):

```
51ced4a0  warn on finalise_backfill errors + lock_guard hardening doc
cbbc9059  error: dedupe coded_sql/prefix_message across 5 sites          *
f7d0961c  error: update preamble after [I28] sweep closed the rail
f1f06900  tests: thread app_id through watchdog/dropAbandoned callers
e399eeea  replication_ops: clear consumer-running marker via Drop guard  *
eda96ead  extract first_row_or_internal() helper                         *
c0590506  v8_classes/replication: scope watchdog + dropAbandoned         *
ffb1e101  orchestrator/lock_guard: warn on pg_advisory_unlock errors
808a32af  orchestrator/lock_guard: must_use + louder Drop log
91830cca  replication: drop stale .into_string() after [I28] sweep
0049d9be  sweep Result<_, String> sites in auth/* + replication.rs
bd1e7ce1  orchestrator/lock_guard: defer released-flag flip
```

Starred entries are the four perf-relevant commits the task brief flagged.

---

## 1. Verify recent commits' perf neutrality

### `e399eeea` — `ConsumerRunningGuard` Drop guard

**File:** `crates/plugin-db/src/replication_ops.rs:264-284`.

```
264  struct ConsumerRunningGuard {
265      app_id: String,
266  }
267  impl Drop for ConsumerRunningGuard {
268      fn drop(&mut self) {
269          crate::context::with_mut(|c| {
270              c.unmark_consumer_running(&self.app_id)
271          });
272      }
273  }
274  let app_for_task = app_id.clone();
275  crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
276  compio::runtime::spawn(async move {
277      let _guard = ConsumerRunningGuard {
278          app_id: app_for_task,
279      };
280      crate::wal_consumer::run_supervised(consumer).await;
281      ...
282  })
283  .detach();
```

The guard runs ONCE per `startReplicationConsumer()` (boot-time + idempotent;
the prior `is_consumer_running` short-circuit at line 195-207 returns before
spawning a second one). Cost added vs. pre-commit:

- `let app_for_task = app_id.clone()` (line 274) — 1 extra `String` alloc
  beyond the original `app_id` move. Cheap, boot-only.
- `let _guard = ConsumerRunningGuard { app_id: app_for_task }` —
  zero-sized wrapping of a `String` field; LLVM should fold it. The Drop
  impl is one `RefCell::borrow_mut` + one `HashMap::remove(&str)`
  (`unmark_consumer_running`). Both run once per consumer lifetime
  (graceful exit OR panic-unwind), which is far less than once per CRUD
  call.

Net: 1 extra String clone per `startReplicationConsumer()` (boot-only,
not on the CRUD hot path). Zero ongoing cost. **Confirmed perf-neutral
on the hot path.**

Verification: `replication_ops.rs:264-284` + the idempotent early-return
at `:195-207`. Bench: unknown — needs measurement (would want a
`db_start_consumer_idempotent` test that times the second call; the
first-call cost is dominated by the SQL provisioning, not the guard).

---

### `cbbc9059` — coded_sql / prefix_message dedup across 5 sites

**File (helper):** `crates/plugin-db/src/error.rs:327-345` (`prefix_message`),
`:357-361` (central `coded_sql`).

**Sample call sites (3):**

```
audit.rs:58           fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
audit.rs:59               crate::error::coded_sql(&format!("audit: {context}"), e)
audit.rs:60           }
auth/bootstrap.rs:24  fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
auth/bootstrap.rs:25      crate::error::coded_sql(&format!("auth/bootstrap: {context}"), e)
auth/bootstrap.rs:26  }
diff.rs:40            fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
diff.rs:41                crate::error::coded_sql(&format!("diff: {context}"), e)
diff.rs:42            }
```

**Body of the central helper** (`error.rs:357-361` + `:327-345`):

```
357  pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
358      let mut err: DbError = e.into();
359      prefix_message(&mut err, &format!("{context}: "));
360      err
361  }

327  pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
328      match err {
329          DbError::UniqueViolation { message } | ... | DbError::Internal { message } => {
337              *message = format!("{prefix}{message}");
338          }
343          _ => {}
344      }
345  }
```

Per-call alloc count on the error path now (3 sites sampled — same shape
at all 5):

- Wrapper: 1 `format!("audit: {context}")` → 1 `String`.
- Central `coded_sql`: 1 `format!("{context}: ")` → 1 `String`.
- `prefix_message`: 1 `format!("{prefix}{message}")` → 1 `String`
  (in-place reassignment of the variant's `message`).

That's **3 String allocs per error-path call** post-dedup, vs **1 String
alloc** pre-dedup (the inlined `format!("diff: {context}: {message}")`
shown in `git show cbbc9059 -- crates/plugin-db/src/diff.rs`). Two extra
allocs introduced by the abstraction layering.

Severity: **MINOR / cold path.** All 5 call sites fire only on
`compio_postgres::Error` propagation (SQL failure). The CRUD hot path
on success allocates none of these. Operator-visible cost change is
zero unless the workload is error-storm-bound. Variant-walking cost is
unchanged (same match arms, same `*message = ...` assignment).

**Confirmed perf-neutral on the hot path; small cold-path regression
of 2 extra Strings per SQL error.** See N6-M10 below for the optional
buyback (collapse the two `format!`s into one).

Verification: helper body at `error.rs:327-361` verbatim + 3 sampled
wrappers at `audit.rs:58-60`, `auth/bootstrap.rs:24-26`, `diff.rs:40-42`
+ pre-image from `git show cbbc9059 -- crates/plugin-db/src/diff.rs`.
Bench: unknown — needs measurement (an error-path bench would surface
this).

---

### `c0590506` — watchdog / dropAbandoned `$1`-bound prefix

**File:** `crates/plugin-db/src/replication.rs:375-419` (`watchdog_query`),
`:486-580` (`drop_abandoned_slots`), `:125-127` (`slot_name_like_prefix`).

```
125  pub(crate) fn slot_name_like_prefix(app_id: &str) -> Result<String, DbError> {
126      Ok(format!("{}%", slot_name(app_id)?))
127  }
```

`slot_name` (`:109-111`) does another `format!`, so each
`slot_name_like_prefix` call costs 2 String allocs + the sanitisation
work in `sanitise_app_id`.

Call sites:

- `watchdog_query`: 1 call to `slot_name_like_prefix` per invocation (2
  String allocs).
- `drop_abandoned_slots`: 1 call to `slot_name_like_prefix` per
  invocation (2 String allocs) + a `floor_bytes.to_string()` for the
  numeric `$2` bind.

Both functions run from operator-scheduled crons (`db.replication.watchdog()`,
`db.replication.dropAbandoned()`), NOT on the CRUD hot path. The
per-row work inside the result loop (`watchdog_query:402-417`) is
unchanged — same `row.try_get::<_, String>` per column as before.

Measurable cost change vs. pre-`c0590506`: **+2 String allocs per
watchdog call**, **+3 String allocs per dropAbandoned call** (the
extra is the `floor_bytes.to_string()`). All on the cron path; not on
the CRUD hot path.

**Confirmed perf-neutral on CRUD; bounded cost addition on the cron
path.** Important security work that closes the cross-tenant
enumeration; the per-call alloc cost is the correct tradeoff.

Verification: `replication.rs:125-127, 375-419, 486-580`. Bench:
unknown — needs measurement (no cron-path bench exists; cost is
swamped by the SQL round-trip anyway).

---

### `eda96ead` — `first_row_or_internal` helper

**File:** `crates/plugin-db/src/error.rs:378-385`.

```
378  pub(crate) fn first_row_or_internal<'a, R>(
379      rows: &'a [R],
380      op: &'static str,
381  ) -> Result<&'a R, DbError> {
382      rows.first().ok_or_else(|| DbError::Internal {
383          message: format!("{op}: returned no row"),
384      })
385  }
```

Generic over `R`. No `#[inline]` attribute. The `pub(crate)` visibility
limits cross-crate inlining; same-crate inlining is up to LLVM. The
function body is 3 statements (`rows.first()`, `ok_or_else(closure)`,
return). LLVM almost certainly inlines this under `--release` — body
is shorter than the call-overhead — but without `#[inline]` it depends
on inlining heuristics rather than being guaranteed.

Happy-path cost: `rows.first()` + a `?`-unwrap. The closure body
(`format!(...)`) is lazy (`ok_or_else`, not `ok_or`) so the `format!`
alloc only fires when `rows.first()` returns `None`. **Zero-cost on
the success path.**

Call sites (production):

- `audit.rs:314` (`write_audit_row` — runs per audit row insert; deploy
  / register_model path, not per CRUD).
- `audit.rs:600` (`insert_backfill_running` — deploy path).
- `replication.rs:282` (`ensure_publication_and_slot` — boot path).

None on the per-CRUD hot path. **Confirmed perf-neutral.**

Optional polish: adding `#[inline]` would guarantee inlining, but the
present compilation unit has no measured concern. Severity not raised.

Verification: `error.rs:378-385` + grep of call sites (`audit.rs:314,
600`, `replication.rs:282`). Bench: unknown — needs measurement.

---

## 2. Re-walk audit — fresh findings

### CRITICAL — none

No CRITICAL regressions vs. r5. The recent cycle's commits are
correctness / security / abstraction work; their hot-path impact is
zero or negligible. The remaining hot-path costs are the same structural
items r5 flagged.

### IMPORTANT

#### N6-I1 — `row_to_json` / `column_to_json` O(N²) per row (UNCHANGED since N4-I3 / N5-I1)

**File:** `crates/plugin-db/src/v8_bridge.rs:353-361` + every
`row.try_get::<_, T>(name)` / `row.raw_value(name)` call at `:369-495`.

Re-verified at HEAD verbatim:

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

`column_to_json` accepts `name: &str` and resolves the column index
inside `row.try_get` / `row.raw_value`. The `RowIndex for str` impl at
`crates/compio-postgres/src/row.rs:65-82` is a two-pass linear scan
(case-sensitive then case-insensitive). For N columns in the outer
loop × O(N) per `try_get` → O(N²) per row. A 20-column row pays ~400
string compares; 100 rows pays 40 000. Fired on every `find()`,
`findOne`, `insert RETURNING`, `update RETURNING`, `delete RETURNING`.

The `RowIndex for usize` impl at `row.rs:49-61` is O(1), but
`column_to_json` never reaches it.

Why: largest remaining structural hot-path cost on the read path. No
commit between r5 and r6 touched `v8_bridge.rs`.

Fix: take the column index by enumeration and pass `usize` down:

```rust
pub(crate) fn row_to_json(row: &compio_postgres::Row) -> Value {
    let cols = row.columns();
    let mut obj = serde_json::Map::with_capacity(cols.len());   // N6-M6 too
    for (idx, col) in cols.iter().enumerate() {
        let value = column_to_json_by_idx(row, idx, col.type_().oid());
        obj.insert(col.name().to_string(), value);
    }
    Value::Object(obj)
}
```

…and have `column_to_json_by_idx` use `row.try_get::<usize, T>(idx)` /
`row.raw_value::<usize>(idx)` everywhere. Single-file change inside
plugin-db (the index variant is already present in compio-postgres via
the `RowIndex for usize` impl).

Verification: `v8_bridge.rs:340-497` verbatim + `compio-postgres/src/row.rs:65-82`.
Bench: unknown — needs measurement (would want a `db_find_wide_table`
bench against a 20+-column table).

---

#### N6-I2 — `$in` / `$nin` placeholder `Vec<String>` + `join` (UNCHANGED since R1 / N5-I2)

**File:** `crates/plugin-db/src/query.rs:1944-1969`.

Re-verified at HEAD verbatim:

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

Why: N-element `$in` allocates N small `String`s + 1 `Vec<String>`
+ 1 `join` String + 1 outer `format!` String. Same on `$nin`. Hot on
every `find({ id: { $in: [...] } })` (canonical "load these rows by
ID" pattern).

Fix: stream into a single buffer via `std::fmt::Write` (sketch from r5
still applies). One alloc regardless of array length.

Verification: `query.rs:1944-1969` verbatim. No diff between r5 and
r6. Bench: unknown — needs measurement.

---

#### N6-I3 — `migrations::exec_fetch_batch` serialises `Vec<Value>` → `String` for the V8 boundary (UNCHANGED since R1 / N5-I3)

**File:** `crates/plugin-db/src/migrations.rs:432-434`.

```
432  let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
433  let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
434  Ok(Value::Array(row_jsons).to_string())
```

Backfill batches are bounded (≤ 10 000 rows per call) and only run
during deploy / schema changes. CRUD hot path is unaffected. Severity
stays IMPORTANT (not CRITICAL); the round-trip is a wasted
String-serialise + re-parse at the V8 boundary.

Fix: change `exec_fetch_batch`'s return type to `Vec<Value>` matching
`exec_query`; let the v8_class wrapper hand it through to V8.

Verification: `migrations.rs:432-434` verbatim. Bench: unknown — needs
measurement.

### MINOR

#### N6-M1 — `wal_consumer::emit_for_tuple` clones `changed_columns` per published frame (UNCHANGED since N4-M1 / N5-M1)

`wal_consumer.rs:565-569`. Re-verified at HEAD.

```
565  let changed_columns: Vec<String> = rel
566      .columns
567      .iter()
568      .map(|c| c.name.clone())
569      .collect();
```

Fix: cache `Vec<Arc<str>>` on `RelationEntry`; clone Arc into the
event. Bench: unknown — needs measurement.

---

#### N6-M2 — `wal_consumer::emit_for_tuple` + `emit_local` clone `app_id` / `collection` per event (UNCHANGED since N4-M2 / N5-M2)

`wal_consumer.rs:574-576` (WAL path) + `:203-228` (`emit_local`
autocommit path) clone `self.app_id` and `rel.table` per event:

```
574  publish(&ChangeEvent {
575      app_id: self.app_id.clone(),
576      collection: rel.table.clone(),
```

Both Strings are stable for the consumer / relation lifetime;
`broker::publish` then does one more `Rc::new(event.clone())` total
which clones them again.

Fix: switch `ChangeEvent.app_id` and `.collection` to `Arc<str>`
(broker is per-thread → `Rc<str>` works too). Cache one `Arc<str>` per
relation on `RelationEntry`; cache `self.app_id` once on `WalConsumer`.
Bench: unknown — needs measurement.

---

#### N6-M3 — `dispatch_aggregate` builds a fallback `Value::Object` unconditionally (UNCHANGED since N4-M3 / N5-M3)

`crud.rs:402-410`. `read_set::record_if_active` short-circuits at
`read_set.rs:357-372` when no capture is active; the unconditional
`captured_filter` construction + `.cloned()` is wasted work in that
common case.

Fix: probe `read_set::is_active()` before the construction. Bench:
unknown — needs measurement.

---

#### N6-M4 — `dispatch_*` write paths allocate two `String`s to move into async (UNCHANGED since R3 / N5-M4)

`crud.rs:218-219, 246-247, 279-280, 308-309, 340-341, 368-369,
514-515, 553-554` (8 dispatch sites × 2 `String` allocs = 16 per
8-op workload). Re-verified at HEAD.

Fix: switch the `Collection` v8-class's stored name / app to `Rc<str>`
so `coll = Rc::clone(&self.name)` is a refcount bump.

---

#### N6-M5 — `value_to_param_inner` clones every `Value::String` (UNCHANGED since R1 / N5-M5)

`query.rs:2074-2083`. Re-verified at HEAD:

```
2074  fn value_to_param_inner(value: &Value) -> String {
2075      match value {
2076          Value::String(s) => s.clone(),
```

Structural — fixing requires threading a lifetime through `BuiltQuery`.

---

#### N6-M6 — `row_to_json` `serde_json::Map::new()` without capacity hint (UNCHANGED since R3 / N5-M6)

`v8_bridge.rs:354`:

```
354  let mut obj = serde_json::Map::new();
```

`with_capacity(row.columns().len())` skips rehashing on the typical
8-12 column case. The fix is folded into N6-I1 above.

---

#### N6-M7 — `build_aggregate` unconditionally allocates `agg_exprs` HashMap (UNCHANGED since R2)

`query.rs:1480`.

---

#### N6-M8 — `tuple_to_map` allocates `"NULL".to_string()` per NULL column (UNCHANGED since R2 / N5-M8)

`wal_consumer.rs:608-610`. Behind `has_subscribers` gate so only fires
on subscribed tables.

```
608  TupleColumn::Null => {
609      out.insert(col.name.clone(), "NULL".to_string());
610  }
```

Fix: lift `"NULL"` to `const &'static str` + `Cow<'static, str>` on
the value side.

---

#### N6-M9 — `exec.rs::emit_for_rows` builds per-row tuple HashMap (UNCHANGED since N5-M9)

`exec.rs:189-249`. After the `has_subscribers` gate (`:201-205`), the
per-row work (`:206-249`) — `Vec<String>` of column names + `HashMap<String,
String>` of stringified values — fires once per affected row. For a
5-column UPDATE matching 100 rows: ~12 small Strings × 100 = ~1200
allocs per CRUD call to `updateMany` with a wide filter.

Fix: push the alloc cost behind a second gate ("is there a subscriber
whose predicate touches this column?") by restructuring the broker's
accept path to take the original row Value rather than a pre-built
text map.

Verification: `exec.rs:189-249` verbatim. Bench: unknown — needs
measurement.

---

#### N6-M10 — NEW: dedup helper introduces 2 extra String allocs per SQL-error path

**File:** `crates/plugin-db/src/error.rs:357-361` + the 5 wrapper sites
(`audit.rs:58-60`, `auth/bootstrap.rs:24-26`, `auth/keys.rs:41-43`,
`auth/session.rs:35-37`, `diff.rs:40-42`).

The dedup commit (`cbbc9059`) collapses 5 copies of the prefix-walker
into one but adds two `format!` calls on the way:

1. Each wrapper: `format!("audit: {context}")` (or equivalent) — alloc 1.
2. Central `coded_sql`: `format!("{context}: ")` — alloc 2.
3. `prefix_message`: `*message = format!("{prefix}{message}")` — alloc 3.

Pre-dedup: a single inline `*message = format!("diff: {context}: {message}")`
— alloc 1.

Net cost change: **+2 `String` allocs per SQL-error path call**. Fires
only on `compio_postgres::Error` propagation (every `map_err(|e|
coded_sql(...))` site). Zero impact on CRUD success path.

Severity: **MINOR / cold path.** The cost change is real but
operator-invisible unless errors are storming. Worth recording so a
future cycle can fold the work back together.

Why: the abstraction's contract is "compose a per-module prefix + a
per-call context". The fix is to keep the layering but build the
combined prefix string in one shot, e.g. by changing the central
helper's signature so callers pass the parts:

```rust
// error.rs
pub(crate) fn coded_sql_with_module(
    module: &'static str,
    context: &str,
    e: compio_postgres::Error,
) -> DbError {
    let mut err: DbError = e.into();
    prefix_message_with_parts(&mut err, module, context);
    err
}
fn prefix_message_with_parts(err: &mut DbError, module: &'static str, ctx: &str) {
    if let DbError::UniqueViolation { message } | ... | DbError::Internal { message } = err {
        *message = format!("{module}: {ctx}: {message}");
    }
}

// audit.rs (wrapper)
fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    crate::error::coded_sql_with_module("audit", context, e)
}
```

One `format!` per error call (same as pre-dedup) while keeping the
single match-arm contract in `error.rs`. Two units of work elsewhere:
update `prefix_message`'s 4 standalone callers in `replication.rs:201-205,
214-217, 261-273, 396-400, 538-542, 568-573` to use the new helper or
keep a 2-arg overload.

Verification: helper at `error.rs:327-361` + pre-image from
`git show cbbc9059 -- crates/plugin-db/src/diff.rs` + sample wrappers at
`audit.rs:58-60`, `auth/bootstrap.rs:24-26`, `diff.rs:40-42`. Bench:
unknown — needs measurement (only an error-storm bench would surface
the magnitude).

---

## 3. CRUD path full audit — dominant cost per call

Walked end-to-end post-r5 + post-recent-commits. No structural changes
to the alloc accounting since r5 (the recent commits touch error /
replication / orchestrator paths, not CRUD).

### `findOne(filter)` allocation walk

Same as r5 — abbreviated here, full walk in r5 §3:

1. `record_if_active(collection, &filter)` — gated by `is_active()`.
2. `validate_collection` — 0 prefix-check allocs (closed by [I36] at
   r5; re-verified at `query.rs:61-99`).
3. `quote_ident(app_id)` + `quote_ident(collection)` — 2 String allocs.
4. `build_where` — ~1 String alloc per condition.
5. `build_find`'s `format!` — 1 String alloc.
6. `setup_js_promise` — V8 resolver pair (structural).
7. `exec_query` → `param_refs: Vec<&str>` (1 Vec alloc).
8. `rows_to_json_value` → `row_to_json` per row:
   - 1 `serde_json::Map::new()` (no capacity hint, **N6-M6**).
   - per column: 1 `col.name().to_string()` + O(N) name lookup
     (**N6-I1** — O(N²) total).
9. `first_row_or_null` → 1 String alloc (JSON payload).
10. V8: `JSON.parse` — one parse.

Dominant cost: **N6-I1** (O(N²) column lookup) on wide rows; otherwise
the V8 JSON round-trip.

Per-call alloc count on a 4-col row, simple filter, no read-set
capture: ~7 Strings + 1 Map + 1 Vec, plus 16 string compares for the
column lookups. Unchanged vs. r5.

### `insertOne(doc)` (no subscribers)

Same as r5. Dominant cost when no subscribers: same shape as
`findOne`'s row decode + **N6-M4** dispatch-closure allocs (2 Strings).

### `updateMany(filter, update)` (subscribers present)

Same as r5. Dominant cost when subscribed: **N6-M9** (per-row tuple
HashMap build in `emit_for_rows`).

Verification: trace at `crud.rs:208-232` → `exec.rs:136-249` →
`wal_consumer.rs:203-228` → `broker.rs:480-529`. Bench: unknown —
needs measurement.

---

## 4. Schema-installation cost (registerModel)

**File:** `crates/plugin-db/src/orchestrator/register_model/mod.rs:63-104`
+ `crates/plugin-db/src/context.rs:239-248`.

Fast path: `is_model_registered(app_id, collection)` at line 73 →
`context.is_model_registered`:

```
context.rs:239
pub fn is_model_registered(&self, app_id: &str, collection: &str) -> bool {
    let key = format!("{app_id}:{collection}");          // 1 String alloc
    self.registered_models.contains(&key)
}
```

`registerModel` is called once per `Collection` instance at app boot,
not per CRUD request. The `format!` alloc per call is acceptable
cold-path cost (unchanged since r4 §4 / r5 §4).

The new orchestrator hardening commits (`bd1e7ce1`, `808a32af`,
`ffb1e101`, the lock_guard module docs in `lock_guard.rs:1-69`) all
target correctness / panic-safety on the cold register path; none
of them runs per CRUD call. The `acquire_advisory_lock` SQL round-trip
dwarfs all Rust-side alloc cost on this path.

No hot-path concern. Same recommendation as r5: opportunistic
`Arc<str>` for `RegisteredModels` keys.

---

## 5. Broker fanout

`Broker::publish` (`broker.rs:480-529`) — re-verified at HEAD:

1. Two-level lookup: O(1) on `app_id`, O(1) on `collection`. Both via
   `Borrow<str>` impl on `String` keys → no alloc.
2. `subs.retain(|s| !s.is_closed())` — O(subs_on_this_collection);
   GCs closed entries in-place.
3. `Rc::new(event.clone())` — once per publish total. **Structural
   cost** — clones the `ChangeEvent` including `new_tuple` HashMap.
4. Loop `for s in subs.iter()`: per subscriber, `s.accepts(&shared)`
   short-circuits on first matching predicate; `s.push(...)` is one
   `VecDeque::push_back` + one `Rc::clone` (refcount bump).
5. Bucket pruning at `:523-528`: drop the per-collection Vec if it
   empties; drop the per-app inner HashMap if its last collection
   emptied.

Total cost: O(subs × predicate_entries). Bounded by subscriber count
and per-subscription read-set size — both user-controlled. No
pathological fan-out shape.

The only avoidable cost on this path remains the `event.clone()` at
line 504 (the `new_tuple` HashMap clone). That's structural unless the
broker contract changes to hand subscribers a view rather than an
owned event — and even then, `Subscription` queues outlast the
publish call (per-subscriber `VecDeque<SubscriptionMessage>`), so the
event must outlive the publish.

**Verdict: bounded, well-behaved. No regression vs. r5.** The pruning
test `publish_drops_per_app_map_when_last_collection_empties` at
`broker.rs:1268-1310` still locks the contract.

---

## 6. WAL consumer hot path

`WalConsumer::dispatch` (`wal_consumer.rs:462-504`) and
`emit_for_tuple` (`:524-583`):

Per pgoutput Insert / Update / Delete:

1. `relations.get(&rel_id)` — O(1).
2. `rel.namespace != self.app_id` — String compare (no alloc).
3. **`has_subscribers(&self.app_id, &rel.table)` gate at line 554** —
   the r3/r4 closure. When unsubscribed, return immediately. No
   allocations from this point on.
4. When subscribed:
   - `primary_key_index()` — O(columns) linear scan (no alloc).
   - **N6-M1** — `Vec<String>` of column-name clones.
   - **N6-M8** — `tuple_to_map` builds 2 HashMaps (`new`, optionally
     `old` for UPDATE).
   - **N6-M2** — `publish(&ChangeEvent { app_id: self.app_id.clone(),
     collection: rel.table.clone(), ... })`.
5. Broker `publish` (see §5) — bounded.

Allocation count per subscribed WAL frame (5-col row, INSERT, no old):

- 1 Vec<String> + 5 String clones (changed_columns, **N6-M1**).
- 1 HashMap + 5 String clones (key) + 5 String clones (value) (new_tuple).
- 2 String allocs (app_id + collection, **N6-M2**).
- 1 Rc + 1 HashMap clone inside `broker::publish::event.clone()`.

≈ 20 allocs per WAL frame on a subscribed table. With `Arc<str>`
interning on column names + per-relation cached `app_id`/`collection`,
this could drop to ~5 allocs (the new_tuple value clones are
unavoidable — they are the actual data payload).

Verification: trace at `wal_consumer.rs:462-583` + `broker.rs:480-529`.
Bench: unknown — needs measurement.

---

## 7. Top-3 next-priority items (unchanged from r5)

1. **N6-I1** (`row_to_json` O(N²) column lookup): single-file change.
   Largest remaining structural read-path cost.
2. **N6-I2** (`$in`/`$nin` placeholder Vec): trivial one-spot fix.
3. **N6-M9** (per-row tuple HashMap in `emit_for_rows`): scales with
   `affected_rows × columns`; larger refactor (touches broker accept
   contract).

Bonus (cheap):
4. **N6-M10** (collapse the new dedup-introduced double-`format!` on
   the SQL-error path back to one): small operator-cold win that
   buys back the cost from the otherwise-correctness-only
   `cbbc9059`.

---

## 8. Score

**76 / 100** (vs. r5's 76, r4's 74, r3's 70, r2's 57, r1's 44).

Movement vs r5:

- **+0** for `e399eeea` (ConsumerRunningGuard): correctness fix,
  zero ongoing hot-path cost — added 1 String clone on boot per
  consumer.
- **+0** for `c0590506` (watchdog/dropAbandoned `$1`-bound scoping):
  security fix, +2-3 cron-path String allocs, zero CRUD impact.
- **+0** for `eda96ead` (`first_row_or_internal`): cold-path
  deduplication, zero hot-path cost (closure is lazy).
- **−0 (but noted)** for `cbbc9059` (coded_sql dedup): +2 String
  allocs per SQL-error path. Real but operator-cold. Logged as
  N6-M10 and recoverable with a 2-arg helper signature; not enough
  on its own to move the score.

No new hot-path regressions found; no IMPORTANTs closed since r5
either. Score parked at 76.

What's still pulling below 80:

- **N6-I1** — O(N²) column lookup on every read decode.
- **N6-I2** — `$in` placeholder Vec allocation.
- **N6-I3** — migrations backfill String round-trip.
- The continuing absence of a `cargo bench --bench db_*` harness —
  every round writes "unknown — needs measurement" against static-
  analysis counts. A bench landed under `crates/plugin-db/benches/`
  would convert several of these from assertions to numbers.

What would lift past 85:

- N6-I1 fixed (index-based row decode).
- N6-I2 fixed (trivial).
- One DB-touching bench landed (e.g. `db_findone.rs`,
  `db_find_wide.rs`).

What would lift past 90:

- All three IMPORTANTs (N6-I1, N6-I2, N6-I3) closed.
- The N6-M2 / M4 path (`Arc<str>` for `app_id` / `collection` across
  CRUD + WAL + broker) refactored. Cuts per-event allocation by ~3-4
  Strings.
- A documented latency-vs-throughput baseline (μs / op) for the CRUD
  operations so future cycles can detect regressions.

Bench reminder: no plugin-db bench results exist in
`crates/runtime/benches/results-*.txt` (latest is
`2026-05-10-after-eternal-sweep.txt`; everything post-2026-04
exercises fetch/WS/RPC, not the DB path), and no harness exists under
`crates/plugin-db/benches/`. Every "saves N allocs" claim in this
report is a static-analysis count, not a measurement. Score and
severity are qualitative — the next cycle could resolve "is N6-I1 a
5% find or a 50% find on wide rows?" with a single measurement.
