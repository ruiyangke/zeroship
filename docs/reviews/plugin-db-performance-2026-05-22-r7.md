# plugin-db Performance Review — 2026-05-22 r7

Commit: HEAD (`a272d1af`). Cycle baseline: r6 (76 / 100, head `51ced4a0`).
Mode: read-only, no benchmarks executed. No bench artefacts touching the
DB path exist (latest `crates/runtime/benches/results-*.txt` is
`2026-05-10-after-eternal-sweep.txt`; everything post-2026-04 exercises
fetch/WS/RPC). No `crates/plugin-db/benches/` directory. Every
quantitative claim below remains a static-analysis count annotated
`unknown — needs measurement`.

Commits since the r6 baseline (`git log 51ced4a0..HEAD --
crates/plugin-db/`):

```
a272d1af  auth: classify P0001 RAISE via DETAIL token instead of substring  *
aa639715  wal_consumer: WalConsumer::new returns Result<_, DbError>          *
4b2e7046  replication_ops: rewrite ConsumerRunningGuard comment block
70921112  replication_ops: atomic try-claim closes startReplicationConsumer race *
34d209b5  replication_ops: move consumer-running mark inside ConsumerRunningGuard::new
```

Starred entries are the four perf-relevant commits the task brief flagged.
(`34d209b5` is from cycle 06:00 and was covered indirectly by r6; this
review re-walks it for completeness alongside `70921112` which builds on
it.)

---

## 1. Verify recent commits' perf neutrality

### `70921112` — `try_mark_consumer_running` atomic check

**File:** `crates/plugin-db/src/context.rs:419-426` (new helper) +
`crates/plugin-db/src/replication_ops.rs:285-294` (`try_claim`
constructor).

```
context.rs:424
pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
    self.running_consumers.insert(app_id.to_string())
}

replication_ops.rs:285-294
impl ConsumerRunningGuard {
    fn try_claim(app_id: String) -> Option<Self> {
        let won = crate::context::with_mut(|c| {
            c.try_mark_consumer_running(&app_id)
        });
        won.then_some(Self { app_id })
    }
}
```

Cost change vs. r6 pre-image (`mark_consumer_running`):

- Pre-image: 1 `HashSet::insert` returning `()` (the bool was
  discarded inside `mark_consumer_running` — wait, actually
  `mark_consumer_running` already called `.insert(app_id.to_string())`
  and threw the bool away).
- Post-image: `HashSet::insert(app_id.to_string())` returning the same
  bool, now propagated up.

LLVM emits the exact same `HashMap::insert` call site; the only
difference is that the return-value bool now flows up instead of being
dropped. No new alloc, no new branch. The `won.then_some(Self {
app_id })` wrapping is zero-cost (it consumes the input `String` rather
than cloning).

Per-call accounting at HEAD (boot-only, once per
`startReplicationConsumer()` dispatch, gated by the outer
`is_consumer_running` short-circuit at `replication_ops.rs:195-207`):

- 1 `String::to_string()` on `app_id` (unchanged from r6).
- 1 `HashSet::insert` (unchanged).
- 1 boolean branch in `try_claim` (new, 1 instruction).
- 1 `app_for_task = app_id.clone()` at `:302` (unchanged from r6).

**Confirmed perf-neutral.** The whole guard machinery still runs once
per consumer lifetime (boot + supervised exit); zero on the CRUD hot
path.

Verification: `context.rs:419-426` + `replication_ops.rs:282-310`
verbatim. Bench: unknown — needs measurement (a
`db_start_consumer_idempotent` bench would surface the boot-side cost,
but the boot path is dominated by `ensure_publication_and_slot`'s
multi-statement SQL round-trip, not by the guard).

---

### `aa639715` — `WalConsumer::new` returns `Result<_, DbError>`

**File:** `crates/plugin-db/src/wal_consumer.rs:346-368` (new
constructor) + `crates/plugin-db/src/replication_ops.rs:241-250`
(dispatch site).

**Pre-image (from `git show aa639715^:crates/plugin-db/src/replication_ops.rs`):**

```
let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
    Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
    Err(e) => {
        return OpResult::JsValue {
            resolver,
            value: ResolveValue::RejectError(
                DbError::Configuration {
                    code: "not_provisioned",
                    message: e.to_string(),         // 1 String alloc
                }
                .to_op_error(),
            ),
            request_id,
        };
    }
};
```

**Post-image (`replication_ops.rs:241-250`):**

```
let consumer = match crate::wal_consumer::WalConsumer::new(&app_id, &url) {
    Ok(c) => c.with_start_lsn(setup.confirmed_flush_lsn.clone()),
    Err(e) => {
        return OpResult::JsValue {
            resolver,
            value: ResolveValue::RejectError(e.to_op_error()),
            request_id,
        };
    }
};
```

Net cost change on the consumer-startup ERROR path: **−1 String alloc**
(no more `e.to_string()` to stamp into a new `Configuration { message }`).
The `DbError::Configuration { code: "not_provisioned", message: ... }`
struct is also no longer constructed inside replication_ops — it's
either built once inside `WalConsumer::new` (`wal_consumer.rs:347-353`)
for the empty-`db_url` case or it's a `DbError::ValidationFailed` from
`sanitise_app_id` propagating verbatim.

Inside `WalConsumer::new` (`wal_consumer.rs:346-368`):

- `db_url.is_empty()` check — zero alloc.
- `slot_name(app_id)?` / `publication_name(app_id)?` — same calls as
  pre-image, unchanged.
- 2 `String::to_string()`s for `app_id` / `db_url` fields — unchanged.

**Confirmed perf-neutral on the hot path; mild cold-path WIN
(−1 `String` alloc) on the consumer-startup error branch.** Logged as
the r7 inverse of r6's N6-M10 entry.

Verification: pre-image from `git show aa639715^:crates/plugin-db/src/replication_ops.rs`
vs `replication_ops.rs:241-250` + `wal_consumer.rs:346-368` at HEAD.
Bench: unknown — needs measurement (the only path that fires this is
"deploy with missing `DB_URL`" or "deploy with invalid `app_id`", both
cold).

---

### `a272d1af` — P0001 DETAIL classification

**File:** `crates/plugin-db/src/auth/session.rs:174-202`
(`classify_p0001_detail`) + `:229-274` (the `.map_err` body) +
`crates/plugin-db/src/auth/bootstrap.rs:521-560` (PG-side DETAIL
tokens).

**Hot path:** `init_session` runs on session checkout (worker start /
RPC session-mint), **NOT per CRUD call**. Per-call accounting is on
the error branch only.

Per-call accounting on the `.map_err` body at HEAD vs. pre-image:

- Pre-image: `format!("{e}")` (1 String) + N source-walk `format!("{src}")`
  + 3 sequential `msg.contains(...)` substring scans + 1 `coded_sql(...)`
  on fall-through.
- Post-image: `format!("{e}")` (1 String) + N source-walk
  `format!("{src}")` + 1 `classify_p0001_detail(&e)` call (reads
  `e.as_db_error()?.code()` + `e.as_db_error()?.detail()?` — both
  borrow, **zero allocation**) + a 5-arm `&str` match.

**[MINOR] crates/plugin-db/src/auth/session.rs:233-239 — `msg` is now
dead allocation**

```
233      let mut msg = format!("{e}");
234      let mut cur: &dyn std::error::Error = &e;
235      while let Some(src) = std::error::Error::source(cur) {
236          msg.push_str(" | ");
237          msg.push_str(&format!("{src}"));
238          cur = src;
239      }
...
270                  let _ = msg;
271                  coded_sql("init_session", e)
```

  Why: the pre-image (substring-matching branch at
  `git show a272d1af^:crates/plugin-db/src/auth/session.rs`) used `msg`
  for `msg.contains("nonce replay detected")` etc. The new code at
  `a272d1af` switched classification to `classify_p0001_detail` which
  reads `e.as_db_error()?.detail()` directly (borrow-only, zero alloc).
  `msg` is no longer consumed; the `let _ = msg;` at line 270 makes it
  explicit. The entire `format!("{e}")` + source-walk loop now runs
  unconditionally on every `init_session` ERROR (worker session-mint
  failure), allocates 1 String for the outer `format!` plus 1 per
  source-chain link (typically 1-3 for compio_postgres → pgwire →
  io::Error), and the result is discarded.

  Severity: **MINOR / cold path.** Only fires on `init_session`
  errors, NOT per CRUD call. Per-error cost is operator-invisible at
  realistic error rates. Logged as a new regression introduced by
  `a272d1af` because the pre-image consumed `msg`; post-image does
  not. r6's audit did not flag this (the dead `msg` is r7-new).

  Fix:

  ```rust
  .map_err(|e| {
      // Discriminate on the structured DETAIL token first (the
      // SECURITY DEFINER function tags each refusal with a stable
      // machine-readable detail). On a miss, fall through to the
      // generic SQLSTATE classification — no need to materialise
      // the source-chain string for that path; coded_sql consumes
      // `e` directly.
      if let Some((code, op_msg)) = classify_p0001_detail(&e) {
          return DbError::validation(code, op_msg);
      }
      coded_sql("init_session", e)
  })
  ```

  Net win: 1 `String` + N source-walk `String` allocs deleted from
  every `init_session` error path. (No structural change; can be
  collapsed into a single sentence.)

  Verification: pre-image `git show
  a272d1af^:crates/plugin-db/src/auth/session.rs` lines 178-218
  vs HEAD `auth/session.rs:229-274` + `let _ = msg;` at line 270 as
  the smoking gun. Bench: unknown — needs measurement (only an
  init_session-error storm would surface this).

The DETAIL-token PG-side change at `bootstrap.rs:521-560` is one extra
`USING DETAIL = 'token'` clause per `RAISE EXCEPTION` (×5). Cost is
on Postgres' side, paid at deploy-time function install (one-shot per
isolate boot), not per session-mint. **Not on any hot path.**

**Overall verdict on `a272d1af`:** the structural change (substring →
DETAIL) is a security/correctness win. The hot-path impact is zero.
The error-path leaves 1 dead `format!` + source-walk in place — new
MINOR finding above.

---

### `4b2e7046` — comment-only

**File:** `crates/plugin-db/src/replication_ops.rs:256-281` (comment
block before `ConsumerRunningGuard`).

Zero runtime impact. `git show 4b2e7046 -- crates/plugin-db/src/replication_ops.rs`
confirms only `//` lines move; no code structure changes.

**Confirmed perf-neutral.**

---

## 2. Carry-over IMPORTANT re-verification

### N7-I1 — `row_to_json` O(N²) column lookup (UNCHANGED since r3 / r6 N6-I1)

**File:** `crates/plugin-db/src/v8_bridge.rs:353-361` + `column_to_json`
at `:364-497`.

Re-verified verbatim at HEAD:

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

Every `column_to_json` call at `:369-495` dispatches to
`row.try_get::<_, T>(name)` or `row.raw_value(name)`. The `RowIndex
for str` impl at `crates/compio-postgres/src/row.rs:65-82` is a
two-pass linear scan (case-sensitive then case-insensitive). For N
columns in the outer loop × O(N) per `try_get` → **O(N²) per row**.

Why: largest remaining structural hot-path cost on read decode. r6
flagged it; no commit between r6 and HEAD touched `v8_bridge.rs`.

Fix (unchanged from r6 N6-I1): take the column index by enumeration
and pass `usize` down to a `column_to_json_by_idx` variant that uses
`row.try_get::<usize, T>(idx)`. Single-file change.

Verification: `v8_bridge.rs:353-497` verbatim + `compio-postgres/src/row.rs:65-82`.
Bench: unknown — needs measurement (a `db_find_wide_table` bench
against a 20+-column table would quantify this).

---

### N7-I2 — `$in` / `$nin` placeholder `Vec<String>` (UNCHANGED since r1 / r6 N6-I2)

**File:** `crates/plugin-db/src/query.rs:1944-1969`.

Re-verified verbatim:

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

Per N-element `$in`: N small `String` allocs + 1 `Vec<String>` + 1
`join` String + 1 outer `format!` String. Same on `$nin`. Hot on every
`find({ id: { $in: [...] } })` (canonical "load these by ID").

Fix (unchanged from r6 N6-I2): stream into a single buffer via
`std::fmt::Write`. One alloc regardless of array length.

Verification: `query.rs:1944-1969` verbatim. Bench: unknown — needs
measurement.

---

### N7-I3 — `migrations::exec_fetch_batch` Vec→String round-trip (UNCHANGED since r1 / r6 N6-I3)

**File:** `crates/plugin-db/src/migrations.rs:432-434`.

Re-verified verbatim:

```
432  let rows = rows_result.map_err(|e| coded_db("migration fetch", crate::error::DbError::from_pg(&e)))?;
433  let row_jsons: Vec<Value> = rows.iter().map(crate::v8_bridge::row_to_json).collect();
434  Ok(Value::Array(row_jsons).to_string())
```

Backfill batches bounded (≤10 000 rows per call, deploy-time only) —
NOT on the CRUD hot path. Severity stays IMPORTANT (not CRITICAL).

Fix: change `exec_fetch_batch`'s return type to `Vec<Value>` matching
`exec_query`; let the v8_class wrapper hand it through to V8.

Verification: `migrations.rs:432-434` verbatim. Bench: unknown — needs
measurement.

---

## 3. CRUD allocation count — fresh walk

End-to-end walk against `crates/plugin-db/src/crud.rs` →
`exec.rs` → `query.rs` → `v8_bridge.rs` at HEAD. No structural change
since r6; the four recent commits touched
error-classification, consumer-startup, and replication-spawn paths
exclusively. The CRUD code path is identical to r6.

### `findOne(filter)` allocation walk

1. `record_if_active(...)` — gated by `read_set::is_active()` at
   `read_set.rs:357-372`. Zero alloc when no capture is active (common).
2. `validate_collection` — zero prefix-check allocs (closed by [I36] at
   r5; `query.rs:61-99` re-verified).
3. `quote_ident(app_id)` + `quote_ident(collection)` — 2 String allocs.
4. `build_where` — ~1 String alloc per filter condition.
5. `build_find`'s `format!` — 1 String alloc.
6. `setup_js_promise` — V8 resolver pair (structural).
7. `exec_query` → `param_refs: Vec<&str>` (1 Vec alloc).
8. `rows_to_json_value` → `row_to_json` per row:
   - 1 `serde_json::Map::new()` without capacity hint (**N7-M5**,
     r6-known).
   - per column: 1 `col.name().to_string()` + O(N) name lookup
     (**N7-I1** — O(N²) total).
9. `first_row_or_null` → 1 String alloc (JSON payload).
10. V8: `JSON.parse`.

Per-call alloc count on a 4-col row, simple filter, no read-set
capture: ~7 Strings + 1 Map + 1 Vec + 16 string compares for the
column lookups. **Unchanged vs. r6.**

### `insertOne(doc)` (no subscribers)

`crud.rs:208-232`:
- `query::build_insert(app_id, collection, &doc)` — ~3 String allocs
  (quote_ident + format!).
- 2 `String::to_string()` at `:218-219` (**N7-M2** — r6 N6-M4, "two
  Strings to move into async"; 8 dispatch sites total).
- `exec_mutation_with_emit` → if no subscribers: gated short-circuit
  at `exec.rs:201-205` (re-verified — `has_subscribers` check).
- `first_row_or_null` → 1 String alloc.

Dominant cost when no subscribers: dispatch closure (2 Strings) + row
decode (same `N7-I1` O(N²)). **Unchanged vs. r6.**

### `updateMany(filter, update)` (subscribers present)

After the `has_subscribers` gate at `exec.rs:201-205`:
- `emit_for_rows` (`exec.rs:206-249`) — per affected row: build a
  `Vec<String>` of column names + a `HashMap<String, String>` of
  stringified values, then `emit_local(...)` (which clones `app_id`
  and `collection` per event — **N7-M3**, r6 N6-M2). ~12 small String
  allocs per row × N rows.

Dominant cost when subscribed: **N7-M4** (per-row tuple HashMap build
in `emit_for_rows`, r6 N6-M9). Unchanged vs. r6.

Verification: `crud.rs:208-369`, `exec.rs:189-249`,
`wal_consumer.rs:200-228`, `broker.rs:480-529`. Bench: unknown — needs
measurement.

---

## 4. Broker fanout — bounded?

`Broker::publish` at `broker.rs:480-529` re-verified at HEAD:

1. Two-level lookup via `Borrow<str>` — O(1), no alloc.
2. `subs.retain(|s| !s.is_closed())` — O(subs_on_this_collection);
   in-place GC.
3. `let shared = Rc::new(event.clone())` at `:504` — once per publish
   total. **Structural cost** (clones the `ChangeEvent` including the
   `new_tuple` HashMap). Shared across all live subscribers via
   `Rc::clone` (refcount bump), so per-subscriber cost is ~free.
4. `for s in subs.iter()`: per subscriber, `s.accepts(&shared)`
   short-circuits on first matching predicate.
5. Bucket pruning at `:523-528`: drop the per-collection Vec when
   empty; drop the per-app inner HashMap when its last collection
   emptied. **Test contract** at `broker.rs:1268-1310`
   (`publish_drops_per_app_map_when_last_collection_empties`).

Total cost: O(subs × predicate_entries). Bounded by subscriber count
and per-subscription read-set size (both user-controlled). **No
pathological fan-out shape.** Same posture as r6.

The only avoidable cost on this path remains the `event.clone()` at
`:504` (HashMap clone). Structural unless the broker contract changes
to hand subscribers a view rather than an owned event — and even then,
`Subscription` queues outlast the publish call.

**Verdict: bounded, well-behaved. No regression vs. r6.**

Verification: `broker.rs:480-529` verbatim. Bench: unknown — needs
measurement.

---

## 5. WAL consumer hot path — `dispatch` / `emit_for_tuple`

`crates/plugin-db/src/wal_consumer.rs:486-607` re-verified at HEAD.

Per pgoutput Insert / Update / Delete:

1. `relations.get(&rel_id)` — O(1).
2. `rel.namespace != self.app_id` — String compare (no alloc).
3. **`has_subscribers(&self.app_id, &rel.table)` gate at `:578`** —
   the r3/r4 closure. When unsubscribed: return immediately, ZERO
   allocations from this point on. (This is the structural perf
   improvement — most tables are unsubscribed in typical apps.)
4. When subscribed:
   - `primary_key_index()` — O(columns) linear scan, no alloc.
   - **N7-M1** — `Vec<String>` of column-name clones (`:589-593`,
     r6 N6-M1).
   - **N7-M6** — `tuple_to_map` builds new_tuple HashMap (+ optionally
     old_tuple); per-NULL allocates `"NULL".to_string()` (r6 N6-M8).
   - **N7-M3** — `publish(&ChangeEvent { app_id: self.app_id.clone(),
     collection: rel.table.clone(), ... })` at `:598-606`. Two
     String clones per event (r6 N6-M2).
5. `broker::publish` (§4) — bounded.

Allocation count per subscribed WAL frame (5-col row, INSERT, no old):

- 1 Vec<String> + 5 String clones (changed_columns, N7-M1).
- 1 HashMap + 5 String clones (key) + 5 String clones (value)
  (new_tuple).
- 2 String allocs (app_id + collection, N7-M3).
- 1 Rc + 1 HashMap clone inside `broker::publish::event.clone()`.

≈ 20 allocs per WAL frame on a subscribed table. With `Arc<str>`
interning on column names + per-relation cached `app_id`/`collection`,
this could drop to ~5 allocs (the new_tuple value clones are
unavoidable — they ARE the data payload). **Unchanged vs. r6.**

Verification: `wal_consumer.rs:486-607` + `broker.rs:480-529`. Bench:
unknown — needs measurement.

---

## 6. N6-M10 follow-up — coded_sql wrapper chain (3 allocs vs 1)

Re-verified at HEAD: `error.rs:327-361` is **unchanged since r6**.

```
327  pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
328      match err { ... => *message = format!("{prefix}{message}"); ... }
345  }
357  pub(crate) fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
358      let mut err: DbError = e.into();
359      prefix_message(&mut err, &format!("{context}: "));
360      err
361  }
```

The 5 wrappers at `audit.rs:58-60`, `auth/bootstrap.rs:24-26`,
`auth/keys.rs:41-43`, `auth/session.rs:35-37`, `diff.rs:40-42` each do
`format!("audit: {context}")` (or equivalent) before passing in,
producing the **3 allocs per error path** measured in r6:

1. Wrapper: `format!("audit: {context}")` — alloc 1.
2. Central `coded_sql`: `format!("{context}: ")` — alloc 2.
3. `prefix_message`: `*message = format!("{prefix}{message}")` —
   alloc 3.

vs. the pre-dedup single inline `format!("diff: {context}: {message}")`
— alloc 1.

**Status: STILL OPEN.** No commit in the r6→r7 window touched
`error.rs::coded_sql`, `prefix_message`, or any of the 5 wrapper sites
(`git log 51ced4a0..HEAD -- crates/plugin-db/src/error.rs
crates/plugin-db/src/audit.rs crates/plugin-db/src/diff.rs
crates/plugin-db/src/auth/bootstrap.rs crates/plugin-db/src/auth/keys.rs
crates/plugin-db/src/auth/session.rs` → returns only `a272d1af` which
edits the .map_err body, not the wrapper).

Severity: **MINOR / cold path** (only fires on SQL errors). Fix sketch
from r6 (2-arg `coded_sql_with_module(module, ctx, e)` helper)
still applies. Logged as **N7-M7** below.

Verification: `error.rs:327-361` verbatim at HEAD; sample wrappers
unchanged at `audit.rs:58-60`, `auth/bootstrap.rs:24-26`,
`diff.rs:40-42` (no diff vs. r6). Bench: unknown — needs measurement.

---

## 7. Re-walk audit — fresh findings

### CRITICAL — none

No CRITICAL regressions vs. r6. The four targeted commits are
correctness / security / typing work; their hot-path impact is zero
(`70921112`, `4b2e7046`, `a272d1af`) or a small cold-path win
(`aa639715`).

### IMPORTANT — none new

The three carry-over IMPORTANTs (N7-I1, N7-I2, N7-I3 above) remain
the dominant remaining structural costs. No new IMPORTANT raised.

### MINOR

#### N7-M0 (NEW r7) — dead `msg` build on every `init_session` error

**File:** `crates/plugin-db/src/auth/session.rs:233-239`.

Detail under §1 above. The pre-image (substring-matching branch) USED
`msg` for classification; `a272d1af` switched classification to
`classify_p0001_detail(&e)` (which reads `e.as_db_error()?.detail()` —
zero-alloc) but left the `format!("{e}")` + source-walk loop in
place. `let _ = msg;` at line 270 explicitly admits the result is
discarded.

  Why: 1 `format!("{e}")` + N `format!("{src}")` per init_session
  ERROR (N = source-chain depth, typically 1-3 for compio_postgres).
  Cold path — only fires on session-mint failures, not per CRUD.

  Fix:

  ```rust
  .map_err(|e| {
      if let Some((code, op_msg)) = classify_p0001_detail(&e) {
          return DbError::validation(code, op_msg);
      }
      coded_sql("init_session", e)
  })
  ```

  Buyback: 1 + N String allocs per error.

  Verification: pre-image `git show
  a272d1af^:crates/plugin-db/src/auth/session.rs` (which consumed
  `msg`) vs. HEAD `auth/session.rs:229-274` (`let _ = msg;` at line
  270). Bench: unknown — needs measurement (only an init_session-error
  storm would surface).

#### N7-M1 — `wal_consumer::emit_for_tuple` clones `changed_columns` per published frame (UNCHANGED since r6 N6-M1)

`wal_consumer.rs:589-593`. Fix: cache `Vec<Arc<str>>` on
`RelationEntry`; clone Arc into the event.

#### N7-M2 — `dispatch_*` write paths allocate two `String`s to move into async (UNCHANGED since r6 N6-M4)

`crud.rs:218-219, 246-247, 279-280, 308-309, 340-341, 368-369,
514-515, 553-554` — 8 dispatch sites × 2 String allocs. Fix: switch
the `Collection` v8-class's stored name / app to `Rc<str>` so `coll =
Rc::clone(&self.name)` is a refcount bump.

#### N7-M3 — `emit_for_tuple` + `emit_local` clone `app_id` / `collection` per event (UNCHANGED since r6 N6-M2)

`wal_consumer.rs:598-606` (WAL path) + `:220-228` (autocommit path).
Both Strings are stable for consumer / relation lifetime. Fix:
`Arc<str>` (or `Rc<str>` — broker is per-thread).

#### N7-M4 — `exec.rs::emit_for_rows` builds per-row tuple HashMap (UNCHANGED since r6 N6-M9)

`exec.rs:189-249`. Fires once per affected row on subscribed tables.
For 5-col UPDATE matching 100 rows: ~1200 small String allocs per
`updateMany`. Fix: push the alloc behind a second gate ("does any
subscriber's predicate touch this column?").

#### N7-M5 — `row_to_json` `serde_json::Map::new()` without capacity hint (UNCHANGED since r3 / r6 N6-M6)

`v8_bridge.rs:354`. Folded into the N7-I1 fix.

#### N7-M6 — `tuple_to_map` allocates `"NULL".to_string()` per NULL column (UNCHANGED since r2 / r6 N6-M8)

`wal_consumer.rs:608-610`. Behind `has_subscribers` gate; fires only
on subscribed tables. Fix: `Cow<'static, str>` on the value side with
a `const NULL: &str = "NULL"`.

#### N7-M7 — `coded_sql` 3-alloc wrapper chain (UNCHANGED since r6 N6-M10)

Detail under §6. Cold-path; STILL OPEN. r7's `a272d1af` did not
touch the wrappers or the central helper.

#### N7-M8 — `value_to_param_inner` clones every `Value::String` (UNCHANGED since r1)

`query.rs:2074-2083`. Structural; requires threading a lifetime
through `BuiltQuery`.

#### N7-M9 — `dispatch_aggregate` builds a fallback `Value::Object` unconditionally (UNCHANGED since r6 N6-M3)

`crud.rs:402-410`. Fix: probe `read_set::is_active()` before the
construction.

#### N7-M10 — `build_aggregate` unconditionally allocates `agg_exprs` HashMap (UNCHANGED since r2)

`query.rs:1480`.

---

## 8. Schema-installation cost (registerModel) — re-checked

`crates/plugin-db/src/orchestrator/register_model/mod.rs:63-104` +
`context.rs:239-248` re-verified at HEAD.

Fast path: `is_model_registered(app_id, collection)` →
`context.rs:239-248`:

```
pub fn is_model_registered(&self, app_id: &str, collection: &str) -> bool {
    let key = format!("{app_id}:{collection}");          // 1 String alloc
    self.registered_models.contains(&key)
}
```

Called once per `Collection` instance at app boot, NOT per CRUD
request. Acceptable cold-path cost. **Unchanged vs. r6.**

No hot-path concern. Opportunistic future improvement: `Arc<str>` for
`RegisteredModels` keys (deferred, low ROI).

---

## 9. Top-3 next-priority items

1. **N7-I1** — `row_to_json` O(N²) column lookup. Single-file change;
   largest remaining structural read-path cost.
2. **N7-I2** — `$in` / `$nin` placeholder Vec. Trivial one-spot fix.
3. **N7-M4** — per-row tuple HashMap in `emit_for_rows`. Scales with
   `affected_rows × columns`; larger refactor (touches broker accept
   contract).

Cheap buy-back:
4. **N7-M0** — dead `msg` build on `init_session` error. ~5-line
   delete. Cold-path but a clean buyback for a regression introduced
   by `a272d1af`.
5. **N7-M7** — collapse coded_sql double-`format!` back to one. Small
   operator-cold win.

---

## 10. Score

**76 / 100** (vs. r6's 76, r5's 76, r4's 74, r3's 70, r2's 57, r1's 44).

Movement vs r6:

- **+0** for `70921112` (try_mark atomic): correctness fix on the
  boot path; zero ongoing hot-path cost.
- **+0** for `aa639715` (WalConsumer::new typed Result): typing
  cleanup; saves 1 String alloc on the cold consumer-startup ERROR
  branch only.
- **−0** for `a272d1af` (P0001 DETAIL classification): correctness/
  security win on the cold init_session error path; ALSO leaves a
  dead `format!("{e}")` + source-walk (N7-M0) that the prior
  substring path required and the new DETAIL path does not. Real
  but operator-cold; not enough on its own to move the score.
- **+0** for `4b2e7046` (comment-only).

No new IMPORTANTs found, no IMPORTANTs closed. Score parked at 76.

What's still pulling below 80:

- **N7-I1** — O(N²) column lookup on every read decode.
- **N7-I2** — `$in` placeholder Vec allocation.
- **N7-I3** — migrations backfill String round-trip.
- The continuing absence of a `cargo bench --bench db_*` harness —
  every round writes "unknown — needs measurement" against static-
  analysis counts. A bench landed under `crates/plugin-db/benches/`
  would convert several of these from assertions to numbers.

What would lift past 85:

- N7-I1 fixed (index-based row decode).
- N7-I2 fixed (trivial).
- One DB-touching bench landed (e.g. `db_findone.rs`,
  `db_find_wide.rs`).

What would lift past 90:

- All three IMPORTANTs (N7-I1, N7-I2, N7-I3) closed.
- The N7-M2 / M3 path (`Arc<str>` for `app_id` / `collection` across
  CRUD + WAL + broker) refactored. Cuts per-event allocation by ~3-4
  Strings.
- A documented latency-vs-throughput baseline (μs / op) for the CRUD
  operations so future cycles can detect regressions.

**Anti-fabrication reminder:** no plugin-db bench results exist in
`crates/runtime/benches/results-*.txt` (latest is
`2026-05-10-after-eternal-sweep.txt`; everything post-2026-04
exercises fetch/WS/RPC, not the DB path), and no harness exists under
`crates/plugin-db/benches/`. Every "saves N allocs" claim in this
report is a static-analysis count, not a measurement. Score and
severity are qualitative.
