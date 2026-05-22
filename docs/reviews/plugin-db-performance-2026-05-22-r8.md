# plugin-db Performance Review — 2026-05-22 r8

Commit: HEAD (`f6043126`). Cycle baseline: r7 (`a272d1af`, 76 / 100).
Mode: read-only, no benchmarks executed.

Bench reality check (anti-fabrication baseline). No
`crates/plugin-db/benches/` directory exists. The newest
`crates/runtime/benches/results-*.txt` is `2026-05-10-after-eternal-sweep.txt`,
and a `grep -l -i "plugin-db\|plugin_db\|crud\|findOne\|insertOne\|zeroship.db"`
across `results-2026-05-*.txt` returns zero matches. **Every quantitative
claim below is a static-analysis count annotated `unknown — needs
measurement`.**

Commits since the r7 baseline
(`git log a272d1af..HEAD -- crates/plugin-db/`):

```
f6043126  plugin-db: SQLSTATE-typed checks + classify_detail tests
deeefe18  migrations: route coded_db through the shared prefix_message
386f9bf5  add structural test for [I42] + ConsumerRunningGuard lifecycle tests
                   (lazy then(|| ...) — see §1.3 below)
e5315083  dead-code + docstring cleanups (perf r7 N7-M0)
```

All four are perf-relevant or perf-neutral per the task brief. Audit
walks each below, then re-verifies r7 carry-overs and re-walks the hot
paths fresh.

---

## 1. Verify cycle commits' perf impact

### 1.1 `e5315083` — perf r7 N7-M0 closed

**File:** `crates/plugin-db/src/auth/session.rs:242-262` (`.map_err`
body at HEAD).

Verified at HEAD:

```
242  .map_err(|e| {
... (comment block explaining the substring→DETAIL switch)
258      match classify_p0001_detail(&e) {
259          Some((code, op_msg)) => DbError::validation(code, op_msg),
260          None => coded_sql("init_session", e),
261      }
262  })
```

The pre-image at `a272d1af` built `let mut msg = format!("{e}")` plus a
source-chain walk and then discarded the result via `let _ = msg;`. The
HEAD body does not allocate at all — `classify_p0001_detail(&e)` reads
`e.as_db_error()?.code()` and `.detail()` borrow-only.

**Cost change:** `−1 String` (outer `format!`) `− N Strings` (source
chain, typically 1-3 links for compio_postgres → pgwire → io::Error)
per `init_session` ERROR. Cold path — only fires on session-mint
failures, not per CRUD call.

**Status: r7 N7-M0 CLOSED.** Net WIN on the cold init_session error
path; no impact on hot CRUD.

Verification: `auth/session.rs:242-262` verbatim at HEAD vs. the r7
report's quoted pre-image. Bench: unknown — needs measurement.

---

### 1.2 `deeefe18` — `migrations::coded_db` routed through shared `prefix_message`

**File:** `crates/plugin-db/src/migrations.rs:80-96`.

```
87  fn coded_db(context: &str, e: crate::error::DbError) -> OpError {
88      let mut db_err = e;
89      // `message` already starts with "db: " from walk_pg_chain;
90      // prepend only the lifecycle context phrase to avoid the
91      // doubly-prefixed "db: {context} failed: db: ..." output
92      // (restored from 60ca1ad6, silently reverted by ed697c45,
93      // re-restored by dec2bd42).
94      crate::error::prefix_message(&mut db_err, &format!("{context}: "));
95      db_err.to_op_error()
96  }
```

The pre-image (`git show deeefe18^:crates/plugin-db/src/migrations.rs`
≈ lines 86-105) open-coded the same variant-walk match arms now in
`crate::error::prefix_message`. The body shrank from ~20 lines to
delegation; the **allocation count on the error path is unchanged**:

- 1 `format!("{context}: ")` (alloc 1)
- 1 `format!("{prefix}{message}")` inside `prefix_message` (alloc 2)

Same 2-alloc shape as before, same call surface (`coded_db("migration
row UPDATE", e)`). Pure refactor — closes the last copy of N6-M10's
duplicated variant-walker (cbbc9059 collapsed five, deeefe18 the
sixth).

**Status: perf-neutral.** This is an architecture-r8 cleanup, not a
perf change. Note that the **3-alloc `coded_sql` wrapper-chain shape
(N7-M7)** is a DIFFERENT pattern (the 5 module-prefix wrappers in
`audit.rs`, `auth/{bootstrap,keys,session}.rs`, `diff.rs` — see §6); it
remains OPEN. `coded_db` is a single-entry helper (no module-prefix
wrapper above it), so the 3-alloc chain does not apply here.

Verification: `migrations.rs:80-96` verbatim at HEAD. Bench: unknown
— needs measurement (only fires on migration-lifecycle SQL errors,
which are cold).

---

### 1.3 `f6043126` — SQLSTATE-typed checks in `replication.rs`

**File:** `crates/plugin-db/src/replication.rs:215-227` (42710 site) +
`:259-274` (55000 site).

Verified at HEAD (line 215-227):

```
if let Err(e) = pool.execute(&pub_sql, &[]).await {
    let is_duplicate_object = e
        .as_db_error()
        .map(|db| {
            db.code() == &compio_postgres::error::SqlState::DUPLICATE_OBJECT
        })
        .unwrap_or(false);
    if !is_duplicate_object {
        let mut err = DbError::from_pg(&e);
        prefix_message(&mut err, "replication: CREATE PUBLICATION: ");
        return Err(err);
    }
}
```

Pre-image used `msg.contains("42710")` after a `format!("{e}")` →
`String` allocation. Same shape applied at the 55000 (object-not-in-
prerequisite-state) check at `:259-274`.

**Cost change on the ERROR path** (both sites):

- Pre-image: 1 `format!("{e}")` String + N `msg.contains(...)` substring
  scans (linear in message length, 2-3 substring tests per error
  branch).
- Post-image: 0 allocations. `e.as_db_error()` is borrow-only;
  `db.code()` returns a `&SqlState` (a static interned reference);
  equality compares two pointers / a small u8 tag.

**Net WIN: −1 `String` alloc + −2-3 substring scans per replication-
bootstrap ERROR.** Cold path — fires once per `ensure_publication_and_slot`
call on the boot or recovery path, NOT per CRUD or per WAL frame.

**Status: small cold-path WIN.** Same correctness-shaped pattern as
auth/session.rs's MAJOR-R5-1; both sites now locale- and formatter-
agnostic. Note: line 275's `let msg = format!("{e:#}");` (visible in
the snippet above) IS still consumed downstream in the `Configuration {
message: format!(...) }` wrap that follows — verified not dead. No
N7-M0-style regression introduced.

Verification: `replication.rs:215-227, 259-274` at HEAD. Bench: unknown
— needs measurement.

---

### 1.4 `386f9bf5` — test commit + `won.then(|| ...)` lazy thunk

**File:** `crates/plugin-db/src/replication_ops.rs` (the `try_claim`
constructor moved from `.then_some(Self { app_id })` to `.then(||
Self { app_id })`).

The fix: `.then_some(...)` evaluates its argument eagerly — on the
lost-race path the temporary `Self` was constructed and immediately
dropped, firing `ConsumerRunningGuard::Drop` which removed the
**winner's** mark from the running-consumers set. `.then(|| ...)`
defers construction to the won path only.

**Cost change:**

- Won path: `bool` → `bool branch` → `Self { app_id }` construction.
  Same one Self construction as pre-image, plus one closure call (LLVM
  inlines a `FnOnce` returning a literal struct; effectively zero
  instructions after inlining).
- Lost path: `bool` → `bool branch` → `None` returned. Pre-image built
  a Self, dropped it, ran Drop (a `HashSet::remove`). Post-image does
  none of that.

**Net on the lost path: −1 Self construction, −1 `HashSet::remove`,
−1 Drop-impl entry.** On the won path: zero cost change after inlining.

This path fires once per `startReplicationConsumer()` boot dispatch.
The lost path is the race-collision branch (rare in production, hot
in race tests). **Not on any CRUD or WAL hot path.**

The user's task brief calls the thunk closure "zero-cost"; that's
true on the won path. The lost path actually saves work — but the
*motivation* for the change is correctness (the latent bug 386f9bf5's
lifecycle tests caught), not perf. The perf delta is incidental and
operator-invisible.

**Status: perf-neutral on won path; small WIN on lost path. Both cold.**

Verification: `replication_ops.rs::try_claim` (≈ line 280-294 at HEAD).
Bench: unknown — needs measurement.

---

## 2. Carry-overs — re-verified at HEAD

### N8-I1 (was N7-I1) — `row_to_json` O(N²) column lookup — UNCHANGED

**File:** `crates/plugin-db/src/v8_bridge.rs:353-361`.

Verified verbatim at HEAD:

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

Each `column_to_json` (`:364-497`) dispatches to
`row.try_get::<_, T>(col.name())` which goes through
`compio-postgres/src/row.rs::RowIndex for str` — a **two-pass linear
scan** (case-sensitive then case-insensitive) per call. For N columns
in the outer loop × O(N) per `try_get` → **O(N²) per row decode.**

  Why: largest remaining structural hot-path cost on every CRUD read.
  Quadratic in column count — at 20 columns this is 400 string
  comparisons per row, vs. 20 with index dispatch. Fires on every
  `findOne` / `find` / `aggregate` result row.

  Fix: enumerate columns once, pass `usize` down to a
  `column_to_json_by_idx` variant using `row.try_get::<usize, T>(idx)`.
  Single-file change in `v8_bridge.rs`. Also folds in N8-M5
  (`Map::with_capacity(N)` instead of `Map::new()`).

  Verification: `v8_bridge.rs:353-497` verbatim + `compio-postgres/src/row.rs`
  `RowIndex for str` impl (linear scan, unchanged since r7). Bench:
  unknown — needs measurement (`db_find_wide_table` against 20+-col
  table would quantify).

**Status: r7 N7-I1 STILL OPEN. Unchanged severity (IMPORTANT).**

---

### N8-I2 (was N7-I2) — `$in` / `$nin` placeholder `Vec<String>` — UNCHANGED

**File:** `crates/plugin-db/src/query.rs:1944-1969`.

Verified verbatim:

```
1948  let placeholders: Vec<String> = arr
1949      .iter()
1950      .map(|v| {
1951          params.push(value_to_param(v));
1952          format!("${}", params.len())
1953      })
1954      .collect();
1955  format!("{col} IN ({})", placeholders.join(", "))
```

Per N-element `$in`: N small `String`s (one per placeholder) + 1
`Vec<String>` + 1 `join` String + 1 outer `format!` String = N + 3
allocs. `$nin` identical at `:1957-1968`.

  Why: hot on every `find({ id: { $in: [...] } })` — the canonical
  "load these by ID" pattern. Common SDK shape. Scales linearly with
  the array size.

  Fix: stream into a single `String` buffer via `std::fmt::Write`:

  ```rust
  let mut buf = String::with_capacity(/* arr.len() * 4 + col.len() + 8 */);
  write!(buf, "{col} IN (").unwrap();
  for (i, v) in arr.iter().enumerate() {
      params.push(value_to_param(v));
      if i > 0 { buf.push_str(", "); }
      write!(buf, "${}", params.len()).unwrap();
  }
  buf.push(')');
  ```

  One alloc regardless of array length.

  Verification: `query.rs:1944-1969` verbatim at HEAD. Bench: unknown
  — needs measurement.

**Status: r7 N7-I2 STILL OPEN. Unchanged severity (IMPORTANT).**

---

### N8-I3 (was N7-I3) — `migrations::exec_fetch_batch` Vec→String round-trip — UNCHANGED

**File:** `crates/plugin-db/src/migrations.rs:426-428`.

Verified verbatim:

```
426  let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
427  let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
428  Ok(Value::Array(row_jsons).to_string())
```

The returned `String` is then handed back to V8 which re-parses it.

  Why: backfill batches are bounded (≤10 000 rows per call) and only
  fire at deploy time — NOT on the CRUD hot path. Still leaves one
  serialise + re-parse round-trip per batch.

  Fix: change `exec_fetch_batch`'s return type to `Vec<Value>`
  matching `exec_query`; let the v8_class wrapper hand it through to
  V8 via `ResolveValue::Json` (one serialise at the boundary).

  Verification: `migrations.rs:426-428` verbatim at HEAD. Bench:
  unknown — needs measurement.

**Status: r7 N7-I3 STILL OPEN. Severity: IMPORTANT (cold but
structural).**

---

## 3. CRUD allocation count — fresh walk

No structural change to `crud.rs` / `exec.rs` / `query.rs` /
`v8_bridge.rs` since r7. The four cycle commits touched
auth/session, migrations (helper plumbing only), replication
SQLSTATE classification, and replication_ops (race fix + tests). The
CRUD code path is byte-identical to r7 for the read/write dispatch
functions.

### `findOne(filter)` — re-walked

`crud.rs:139-166` (verbatim):

1. `crate::read_set::record_if_active(collection, &filter)` —
   `is_active()` short-circuits when no capture; zero alloc in the
   typical request-handler case.
2. `opts.get("orderBy")`, `opts.get("select")` — `Option<&Value>`,
   zero alloc.
3. `setup_js_promise(scope, &state)` — V8 resolver pair (structural).
4. `query::build_find(app_id, collection, &filter, ...)`:
   - `quote_ident(app_id)` + `quote_ident(collection)` — 2 Strings.
   - `build_where` — ~1 String per filter condition (literal scalars).
   - final `format!` for the SELECT statement — 1 String.
5. `state.borrow_mut().spawned_ops.push(Box::pin(run_op(...)))` —
   Box alloc for the future (structural).
6. **During await:** `exec_query` runs:
   - `Vec<&str>` for param refs — 1 Vec alloc (`exec.rs:84`).
   - `run_sql` → driver-side allocations (out of scope).
   - `rows_to_json_value(&rows)` → `row_to_json` per row:
     - `Map::new()` no capacity hint — **N8-M5** (unchanged).
     - per column: 1 `col.name().to_string()` + O(N) name lookup —
       **N8-I1** (O(N²) total).
7. `first_row_or_null(rows)` → 1 String alloc (JSON payload from
   `Value::to_string()`).

Per-call alloc count on a 4-col row, simple filter, no read-set
capture: ~7 Strings + 1 Map + 1 Vec + 16 string compares for column
lookups. **Unchanged vs. r7.**

### `insertOne(doc)` — re-walked

`crud.rs:208-232`:

- `query::build_insert(app_id, collection, &doc)` — ~3 Strings.
- 2 `String::to_string()`s at `:218-219` (**N8-M2** — `let coll =
  collection.to_string(); let app = app_id.to_string();`). These move
  into the `move |bq| async move { ... }` closure body for the spawned
  op.
- `exec_mutation_with_emit` → at `exec.rs:201-205`, the
  `has_subscribers` short-circuit gate skips `emit_for_rows` entirely
  when no subscribers exist. Re-verified at HEAD.
- `first_row_or_null` → 1 String.

Dominant cost when no subscribers: dispatch closure (2 Strings) + row
decode (same O(N²) per N8-I1). **Unchanged vs. r7.**

### `updateMany(filter, update)` with subscribers — re-walked

After the `has_subscribers` gate in `exec.rs:201-205`:
`emit_for_rows` (`exec.rs:189-249`) runs per affected row. Each row
builds:

- `cols: Vec<String>` from `m.keys().cloned().collect()` — ~5 String
  clones per 5-col row.
- `tuple: HashMap<String, String>` from `m.iter().map(|(k,v)| ...)
  .collect()` — ~5 String key clones + 5 stringified values per row.

For 100 rows × 5 cols ≈ 1200 small String allocs per updateMany.
**N8-M4** — unchanged vs. r7.

Verification: `crud.rs:139-450`, `exec.rs:185-250`,
`v8_bridge.rs:353-497`. Bench: unknown — needs measurement.

---

## 4. Broker fanout — bounded?

`broker::publish` at `broker.rs:480-529` re-verified verbatim at HEAD
(byte-identical to r7's snapshot).

1. Two-level lookup via `Borrow<str>` (`event.app_id.as_str()` →
   inner map → `event.collection.as_str()`) — O(1), no alloc.
2. `subs.retain(|s| !s.is_closed())` — O(subs_on_this_collection);
   in-place GC keeps the bucket bounded across publish calls.
3. `let shared = Rc::new(event.clone())` at `:504` — **one** event
   clone per publish total (HashMap clone for `new_tuple` is the
   structural cost), shared across all live subscribers via
   `Rc::clone` (a refcount bump per subscriber, not a deep clone).
4. `for s in subs.iter()`: per subscriber `s.accepts(&shared)` short-
   circuits on first matching predicate entry.
5. **Bucket pruning** at `:523-528`: per-collection Vec dropped when
   empty; per-app inner HashMap dropped when its last collection
   emptied. Test contract at `broker.rs:1268-1310`
   (`publish_drops_per_app_map_when_last_collection_empties`).

**Total cost: O(subs_on_this_collection × predicate_entries).**
Bounded by subscriber count (user-controlled) and per-subscription
read-set size (also user-controlled). **No pathological fan-out shape.**

The remaining avoidable cost is the single `event.clone()` at `:504`
(HashMap clone). Structural unless the broker contract changes to
hand subscribers a borrow / view rather than an owned event — and
even then, `Subscription` queues outlast the publish call (they live
across awaits), so the owned event has to materialise eventually.

**Verdict: bounded, well-behaved. Unchanged vs. r7. No regression
from cycle commits.**

Verification: `broker.rs:480-529` verbatim at HEAD; `git diff
a272d1af..HEAD -- crates/plugin-db/src/broker.rs` → empty. Bench:
unknown — needs measurement.

---

## 5. WAL consumer hot path — re-walked

`crates/plugin-db/src/wal_consumer.rs:486-607` verbatim at HEAD;
byte-identical to r7's snapshot.

Per pgoutput `Insert` / `Update` / `Delete`:

1. `relations.get(&rel_id)` — O(1).
2. `rel.namespace != self.app_id` — String compare, no alloc.
3. **`has_subscribers(&self.app_id, &rel.table)` gate at `:578`** —
   the r3/r4 closure. When unsubscribed, return immediately, **zero
   allocations from this point on.** This is the structural perf
   improvement that keeps the WAL consumer cheap for the majority of
   tables that have no reactive subscribers.
4. When subscribed:
   - `primary_key_index()` — O(columns) linear scan, no alloc.
   - **N8-M1** at `:589-593`: `Vec<String>` of column-name clones.
   - **N8-M6** in `tuple_to_map` at `:622-641`: `HashMap` is built
     with `with_capacity(columns.len())` already (re-verified at
     `:626`); per-column entry insert clones `col.name` and either
     `s` (Text value) or `"NULL".to_string()` (Null sentinel — the
     per-NULL alloc).
   - **N8-M3** at `:598-606`: `app_id: self.app_id.clone(),
     collection: rel.table.clone(), ...` — two String clones per
     event.
5. `broker::publish` (§4) — bounded.

Per-frame alloc count on a subscribed 5-col INSERT, no old tuple:

- 1 Vec<String> + 5 String clones (changed_columns, N8-M1).
- 1 HashMap + 5 key Strings + 5 value Strings (new_tuple, partly
  N8-M6).
- 2 Strings (app_id + collection, N8-M3).
- 1 Rc + 1 HashMap clone inside `broker::publish::event.clone()`
  (§4's structural cost).

≈ 20 allocs per WAL frame on a subscribed table. With `Arc<str>`
interning on column names + per-relation cached `app_id`/`collection`,
this could drop to ~5 allocs (the new_tuple value clones are
unavoidable — they ARE the data payload). **Unchanged vs. r7.**

**Bounded?** Yes:
- Frame rate is bounded by upstream Postgres WAL throughput.
- Per-frame work is O(columns) which is bounded by the table schema.
- The `has_subscribers` gate keeps the unsubscribed-table path at
  near-zero cost.

**Verdict: bounded. No regression from cycle commits.** The four
cycle commits did not touch `wal_consumer.rs` (the lazy thunk fix is
in `replication_ops.rs`, a separate file).

Verification: `wal_consumer.rs:486-641` verbatim at HEAD; `git diff
a272d1af..HEAD -- crates/plugin-db/src/wal_consumer.rs` → empty.
Bench: unknown — needs measurement.

---

## 6. N7-M7 follow-up — `coded_sql` 3-alloc wrapper chain

Re-verified at HEAD: 5 module-prefix wrappers still inline a
`format!("module: {context}")` upstream of the central
`coded_sql(context, e)`, which then `format!`s `"{context}: "` and
calls `prefix_message` which `format!`s `"{prefix}{message}"` —
**three Strings per error path** vs. the pre-dedup single
`format!("module: {context}: {message}")` (one String).

```
audit.rs:58       fn coded_sql(context: &str, e: ...) -> DbError {
                      crate::error::coded_sql(&format!("audit: {context}"), e)
                  }
auth/bootstrap.rs:24  same shape with "auth: bootstrap: ..."
auth/keys.rs:41       same shape with "auth: keys: ..."
auth/session.rs:35    same shape with "auth: session: ..."
diff.rs:40            same shape with "diff: ..."
```

And the central helper:

```
error.rs:362-366
pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
    let mut err: DbError = e.into();
    prefix_message(&mut err, &format!("{context}: "));
    err
}

error.rs:332-350
pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
    match err {
        ...
        | DbError::Internal { message } => {
            *message = format!("{prefix}{message}");
        }
        _ => {}
    }
}
```

Total per error path:
1. Wrapper: `format!("audit: {context}")` — alloc 1.
2. Central: `format!("{context}: ")` — alloc 2.
3. `prefix_message`: `format!("{prefix}{message}")` — alloc 3.

  Why: 3 Strings vs. 1 pre-dedup. Cold path (only fires on SQL
  errors), but it's a clear discipline gap.

  Fix: add a 2-arg `coded_sql_with_module(module, ctx, e)` that
  composes once:

  ```rust
  pub(crate) fn coded_sql_with_module(
      module: &'static str,
      context: &str,
      e: compio_postgres::Error,
  ) -> DbError {
      let mut err: DbError = e.into();
      prefix_message_two(&mut err, module, ": ", context, ": ");
      err
  }
  ```

  with `prefix_message_two` doing a single `format!("{m}{s1}{c}{s2}{msg}")`
  on the matched variants. Net: 1 alloc per error path; 5 wrappers
  collapse to 1-line shims (`coded_sql_with_module("audit", context, e)`).

  Verification: `error.rs:332-366` + `audit.rs:55-60` +
  `auth/bootstrap.rs:21-26` + `auth/keys.rs:38-43` +
  `auth/session.rs:32-37` + `diff.rs:37-42` verbatim at HEAD. Bench:
  unknown — needs measurement.

**Status: STILL OPEN. r7 N7-M7 unchanged.** Severity: MINOR (cold
path). Note `migrations::coded_db` — the deeefe18 cleanup target —
is a SINGLE-entry helper with no module-prefix wrapper above it (it
composes "{context}: " directly), so it incurs only 2 allocs and
is not part of this finding; the finding is about the 5 module-
prefix wrappers above the shared `error::coded_sql`.

---

## 7. Fresh re-walk — new findings

### CRITICAL — none

No CRITICAL regressions vs. r7. The four cycle commits are
correctness/typing/test work; their hot-path impact is zero or a
small cold-path win.

### IMPORTANT — none new

Three carry-overs (N8-I1, N8-I2, N8-I3 above) remain the dominant
structural costs. No new IMPORTANT raised.

### MINOR

#### N8-M9 (carry, was r6/r7 N6-M3/N7-M9) — `dispatch_aggregate` builds fallback `Value::Object` unconditionally

**File:** `crates/plugin-db/src/crud.rs:402-410`.

Verified verbatim at HEAD:

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

  Why: `record_if_active` (`read_set.rs:369-378`) short-circuits when
  `is_active()` is false (no capture context active — the typical
  request-handler case outside `query()`). The `captured_filter`
  expression above ALWAYS runs first: it calls `.cloned()` on the
  inner `$match` value if found (one `Value::clone()` — could
  recurse arbitrarily deep), and otherwise builds `Value::Object(Map::new())`
  (a fresh allocation). Both happen even when the result is
  immediately ignored.

  Per-call cost on `aggregate({...})` outside a query() handler: 1
  `Value::Object` alloc OR 1 `Value::clone()` of the $match filter.

  Fix: probe `read_set::is_active()` before the construction:

  ```rust
  if crate::read_set::is_active() {
      let captured_filter = pipeline.as_array()...
          .unwrap_or_else(|| Value::Object(serde_json::Map::new()));
      crate::read_set::record_if_active(collection, &captured_filter);
  }
  ```

  Saves 1 alloc per `aggregate` call outside `query()` (i.e.
  mutation/action handlers calling `aggregate`).

  Verification: `crud.rs:402-410` + `read_set.rs:357-359, 369-378`.
  Bench: unknown — needs measurement.

**Status: STILL OPEN. Unchanged since r6.** Severity: MINOR.

#### N8-M1 — `wal_consumer::emit_for_tuple` clones `changed_columns` per WAL frame — UNCHANGED

`wal_consumer.rs:589-593`. Fix: cache `Vec<Arc<str>>` on `RelationEntry`;
clone `Arc`s into the event. Unchanged since r6.

#### N8-M2 — dispatch_*` write paths allocate 2 Strings per call to move into async — UNCHANGED

`crud.rs:218-219, 246-247, 279-280, 308-309, 340-341, 368-369,
514-515, 553-554` — 8 dispatch sites × 2 String allocs. Fix:
switch `Collection`'s stored name / app to `Rc<str>`. Unchanged
since r6.

#### N8-M3 — `emit_for_tuple` + `emit_local` clone `app_id`/`collection` per event — UNCHANGED

`wal_consumer.rs:598-606` + `:220-228`. Fix: `Arc<str>` (or
`Rc<str>` — broker is per-thread). Unchanged since r6.

#### N8-M4 — `exec.rs::emit_for_rows` per-row tuple HashMap — UNCHANGED

`exec.rs:189-249`. ~1200 small String allocs per 100-row × 5-col
updateMany. Fix: second gate ("does any subscriber's predicate
touch this column?"). Unchanged since r6.

#### N8-M5 — `row_to_json` `Map::new()` without capacity hint — UNCHANGED

`v8_bridge.rs:354`. Folded into the N8-I1 fix. Unchanged since r3.

#### N8-M6 — `tuple_to_map` allocates `"NULL".to_string()` per NULL column — UNCHANGED

`wal_consumer.rs:633`. Fires only on subscribed tables. Fix:
`Cow<'static, str>` with a `const NULL: &str = "NULL"`. Unchanged
since r2.

#### N8-M7 — `coded_sql` 3-alloc wrapper chain — UNCHANGED

§6 above. Unchanged since r6.

#### N8-M8 — `value_to_param_inner` clones every `Value::String` — UNCHANGED

`query.rs:2074-2083`. Structural; requires threading a lifetime
through `BuiltQuery`. Unchanged since r1.

#### N8-M10 — `build_aggregate` unconditionally allocates `agg_exprs` HashMap — UNCHANGED

`query.rs:~1480`. Unchanged since r2.

---

## 8. Top-3 next-priority items

1. **N8-I1** — `row_to_json` O(N²) column lookup. Single-file change
   in `v8_bridge.rs`; the largest remaining structural read-path cost.
   Also folds in N8-M5.
2. **N8-I2** — `$in` / `$nin` placeholder Vec. Trivial one-spot fix
   via `std::fmt::Write`.
3. **N8-M4** — per-row tuple HashMap in `emit_for_rows`. Scales with
   `affected_rows × columns`. Larger refactor (touches broker accept
   contract).

Cheap buyback (carry-over):
4. **N8-M9** — `dispatch_aggregate` fallback `Value::Object` builds
   even when no read-set capture is active. ~5-line wrap in `if
   read_set::is_active() { ... }`.
5. **N8-M7** — collapse the 5 `coded_sql` wrappers + central helper
   into a 2-arg variant; 1 alloc per SQL error path vs. 3.

---

## 9. Score

**78 / 100** (vs. r7's 76, r6's 76, r5's 76, r4's 74, r3's 70, r2's
57, r1's 44).

Movement vs r7:

- **+1 for `e5315083`** (perf r7 N7-M0 closed): closes a regression
  introduced by `a272d1af` that the r7 audit identified. Cold path
  (init_session ERROR) but it's a clean deletion of a measured-by-
  static-analysis allocation pattern that the new DETAIL-based
  classification path doesn't need. Real closure, not just relabeling.
- **+1 for `f6043126`** (SQLSTATE-typed checks in `replication.rs`):
  −1 `String` alloc + −2-3 substring scans per replication-bootstrap
  ERROR path. Cold (consumer-startup), but a structural pattern
  improvement (locale/formatter-independent) that brings replication.rs
  in line with auth/session.rs MAJOR-R5-1 discipline. Bonus: the
  refactor extracted `classify_detail_token` from
  `classify_p0001_detail` for test surface — same pattern as r6's
  "test-shaped fix that's also a small perf win".
- **+0 for `deeefe18`** (migrations coded_db → shared prefix_message):
  pure refactor. Alloc count unchanged.
- **+0 for `386f9bf5`** (lazy `then(|| ...)` + lifecycle tests):
  correctness fix + test coverage. Perf-neutral on the won path; tiny
  WIN on the lost path. Both cold.

Total +2 → **78 / 100**.

What's still pulling below 80:

- **N8-I1** — O(N²) column lookup on every read decode.
- **N8-I2** — `$in` placeholder Vec allocation.
- **N8-I3** — migrations backfill String round-trip.
- The continuing absence of a `cargo bench --bench db_*` harness —
  every round writes "unknown — needs measurement". A landed DB-touching
  bench (e.g. `db_findone`, `db_find_wide`) would convert several of
  these claims from assertions to numbers.

What would lift past 85:

- N8-I1 fixed (index-based row decode); also folds in N8-M5.
- N8-I2 fixed (trivial).
- One DB-touching bench landed under `crates/plugin-db/benches/`.

What would lift past 90:

- All three IMPORTANTs (N8-I1, N8-I2, N8-I3) closed.
- The N8-M2 / M3 path (`Arc<str>` for `app_id` / `collection` across
  CRUD + WAL + broker) refactored. Cuts per-event allocation by ~3-4
  Strings on the WAL hot path; closes 10 dispatch sites in `crud.rs`.
- A documented latency-vs-throughput baseline (μs / op) for the CRUD
  operations so future cycles can detect regressions.

---

**Anti-fabrication reminder:** no plugin-db bench results exist in
`crates/runtime/benches/results-*.txt` (latest is
`2026-05-10-after-eternal-sweep.txt`; everything post-2026-04
exercises fetch/WS/RPC, not the DB path). No harness exists under
`crates/plugin-db/benches/`. Every "saves N allocs" / "−1 String"
claim in this report is a static-analysis count, not a measurement.
Score and severity are qualitative — no ns/% / speedup numbers
quoted. A bench landed would convert several of these from
assertions to numbers.
