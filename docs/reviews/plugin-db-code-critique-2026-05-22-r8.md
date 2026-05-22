# plugin-db code-quality critique — round 8 (2026-05-22)

**Scope**: `crates/plugin-db/` (29,702 LOC across 29 .rs files, 392
`#[test]` cases).

**Lens**: Rust code quality. Re-audited fresh after r7 (93/100). Four
new commits since r7 (`deeefe18`, `f6043126`, `f1c5184e`, `3d79d2da`)
plus the `3d79d2da` docs sweep.

**Method**: re-ran every grep verification command from the r7
appendix, walked the SQLSTATE rail end-to-end, audited the new
`Configuration { hint }` field for fan-out, re-checked the lock-guard
post-r7 partial fix, and scanned RefCell borrow-across-await fresh.

---

## TL;DR

R7 closed 3 MAJORs (R5-1 substring → SQLSTATE in session.rs; R5-4
broker `w.wake()` under borrow; R6-1 `coded_sql` boilerplate
consolidation), then `f6043126` extended the SQLSTATE-structural
discipline into `replication.rs` (two more substring sites). The
`hint` field threaded through `Configuration` cleanly. `coded_db`
collapsed onto the shared `prefix_message` walker. **R7's outstanding
MAJOR (R5-5 remnant — `released = true` flip even on unlock error)
is still open**, but every other R7 follow-up shipped.

What's left is finer-grain: 3 `Result<_, String>` doc-drift sites the
`error.rs` preamble doesn't enumerate, one `lazy_init_failed` /
`not_configured` code-name asymmetry, the `into_held()` dead-code, a
genuinely-still-MAJOR-by-r7 unlock-confirmation gap, and 4–5 minor
DRY/perf items below the noise floor.

**Score: 94/100.** Up 1 from r7. The headroom past 95 is documentation
drift and rare-path optimisation; investment past 96 is
counterproductive.

---

## Audit dimensions

### 1. RefCell-across-await re-sweep

Re-ran `Grep -n '\.borrow(_mut)?\(\)' crates/plugin-db/src` (85 hits)
and audited every site in `broker.rs` (17), `v8_classes/migration.rs`
(16), `crud.rs` (13). Every borrow scope ends before the next
`.await`:

- `migration.rs:160-169` — `coords.borrow()` cloned into `owner`,
  borrow dropped before `ensure_backend().await`. Pattern repeats
  identically at `:187`, `:204`, `:222`. Verified across all 16
  borrows.
- `broker.rs:307-327` — `push()` holds `RefMut` and calls
  `w.wake()` inside the borrow, but `push()` itself is sync. The R7
  MIN-R5-4 finding was about whether `Waker::wake()` could re-enter
  the same `RefCell` — `Waker::wake()` is documented as not allowed
  to call into the originating executor reentrantly, and the broker
  is single-threaded compio. **R5-4 is correctly closed.**
- `crud.rs:13 borrow sites` — async-method borrows are all
  `borrow().cloned()` patterns that release immediately. No
  across-await.

**No new RefCell-across-await regression.** Sweep clean.

### 2. Unsafe count

7 sites:

```
v8_classes/{collection,db,migration,migrations,replication,subscription,transaction}.rs
```

Every site falls into one of two well-documented patterns:

1. **Weak finalizer drop-Box pattern** (6 sites) —
   `Box::from_raw(raw_addr as *mut T)` in the finalizer closure
   `Weak::with_guaranteed_finalizer` installs. SAFETY comments
   correctly justify: `raw_addr` came from `Box::into_raw` in the
   same call, the closure runs at-most-once on V8 GC reclaim, and the
   `Box::from_raw` reconstructs the original Box for drop.
2. **Pointer-recovery for async state transition** (2 sites in
   `migration.rs:426`, `migration.rs:689`) — `unsafe { &*(addr as
   *const Migration) }` to flip `inner = None` / `Some(owner)` after
   a `.await` resolves. SAFETY comments cite: the `Global` captured
   in the spawned future pins the wrapper, so the Box is still live;
   the macro rejects `&mut self` async, so only `&Migration` is
   recovered and RefCell guards the interior mutation. **Correct
   reasoning.**

`migrations.rs` and the others have a 3rd category — the
allow(unsafe_code) gate at module level is paired with the same
finalizer closure shape. No raw pointer arithmetic, no
`mem::transmute`, no lifetime laundering beyond the documented
addr-round-trip.

**Verdict**: unsafe usage is minimal, every site is justified by a
non-pro-forma SAFETY comment, and the pattern is consistent. No
findings.

### 3. Panic risks

`Grep -n '\.unwrap\(\)' crates/plugin-db/src` — 197 total. Filtered
by `awk` against `#[cfg(test)]` markers:

- **0 unwraps in non-V8-callback production code** outside `query.rs`.
  All 111 in `query.rs` are inside `#[cfg(test)] mod tests`.
- **~22 V8-API unwraps**: `v8::String::new(scope, "...").unwrap()`,
  `v8::PromiseResolver::new(scope).unwrap()`, `v8::Function::new(...
  ).unwrap()`. These fail only on OOM (the V8 allocator is unable to
  produce a tiny string); aborting the worker on OOM is correct
  behaviour. Standard rusty_v8 idiom.
- **`v8_bridge.rs:217`** — `let arr: v8::Local<v8::Array> =
  v.try_into().unwrap();` is guarded by `if v.is_array()` at :216.
  Safe by precondition.
- **`v8_bridge.rs:414`** — `i64::from_be_bytes(bytes.try_into()
  .unwrap())` is guarded by `Some(bytes) if bytes.len() == 8`. Safe
  by length-match arm. Same at `:437` for `i32`/`== 4`.

`Grep -n '\.expect\(' crates/plugin-db/src` — 38 hits:

- **`v8_bridge.rs:95` + 5 other sites** — `expect("RuntimeState not in
  isolate slot")`. Real invariant: the runtime always installs the
  slot before user JS runs. Documented in `v8_bridge::runtime_state`.
- **Test code** — `expect("first claim wins")`, `expect("non-empty")`,
  `expect("Debug impl present")`. All in `#[cfg(test)]`.
- **`lock_guard.rs:208`** — `client.take().expect("OrchestratorLock
  Guard::into_held called on guard with no client")`. By
  construction the field is `Some(_)` after `acquire()` (only
  `release()` / `into_held()` set it to `None`, and both set
  `released = true` first). Test-only constructor `for_test_no_client`
  documents the bypass. Locally correct.

**Indexing**: 0 instances of unchecked `[i]` outside test code or
arms guarded by an explicit length/`is_empty` check (`replication.rs
:308` after `slot_row.is_empty()` at :246, `auth/session.rs:388` after
`s.len() % 2 == 0`).

**Verdict**: panic surfaces are well-defended. Every production unwrap
is either a V8-OOM idiom or a length-checked arm. No findings.

### 4. Error-handling typing — `Result<_, String>` count

R7 claimed the rail had "saturated at 2 sites" (`hex_decode` /
`hex_nibble` in `auth/session.rs`). The current `error.rs` preamble
(line 9-23) documents **two categories** of hold-outs:

1. `validate` stage in `orchestrator::register_model` (Err is the
   `validation_refused` JSON envelope — documented SDK wire
   contract).
2. `hex_decode` / `hex_nibble` in `auth/session.rs` (ASCII parsers
   that never cross an isolate boundary).

Re-running `Grep -nE 'Result<[^,>]+,\s*String\s*>'`:

```
crates/plugin-db/src/lib.rs:351: pub async fn init_pool_async()
crates/plugin-db/src/exec.rs:339: pub async fn exec_mutation_with_emit_for_tests()  // cfg(test)
crates/plugin-db/src/orchestrator/register_model/validate.rs:59  // documented
crates/plugin-db/src/auth/session.rs:381 hex_decode                // documented
crates/plugin-db/src/auth/session.rs:395 hex_nibble                // documented
crates/plugin-db/src/v8_classes/migration.rs:454  parse_commit_spec
crates/plugin-db/src/v8_classes/migration.rs:742  parse_spec
crates/plugin-db/src/v8_classes/migrations.rs:221 parse_name_and_collection
```

**[MAJOR-R8-1] error.rs preamble drift — 4 undocumented `Result<_,
String>` sites.**

Documented (by the preamble at `error.rs:9-23`): validate, hex_decode,
hex_nibble. Actually present: those three PLUS `init_pool_async`,
`parse_commit_spec`, `parse_spec`, `parse_name_and_collection`. The
three `parse_*` are pure V8-input parsers — fine on `Result<_, String>`
because the surrounding code synthesises a JS `TypeError` from the
string. But they're not enumerated. The preamble's "Two ASCII-only
... helpers" wording is a doc-drift bug *and* misleads a contributor
into thinking those parsers must be promoted.

`init_pool_async` is the genuine outlier — see [MAJOR-R8-2].

```
Why: docs claim "saturated at 2 sites"; reality is 7 sites in 3
     categories (validate envelope, V8-spec parsers, init_pool).
Fix: either enumerate the V8-parser category in the preamble
     ("Per-class `parse_*` helpers in v8_classes/{migration,
     migrations}.rs return `Result<SpecParts, String>` because the
     surrounding callback synthesises a JS `TypeError` from the bare
     message"), or promote `parse_*` to `Result<_, OpError>` so the
     boundary keeps the typed-rail discipline end-to-end.
Verification: Grep -nE 'Result<[^,>]+,\s*String\s*>' crates/plugin-db/src
```

**[MAJOR-R8-2] `init_pool_async` still on `Result<_, String>`; two
callsites synthesise `DbError::Configuration` with code drift.**

`lib.rs:351`:

```rust
pub async fn init_pool_async() -> Result<(), String> {
    ...
    .map_err(|e| {
        let mut msg = format!("db: failed to connect: {e}");
        let mut cur: &dyn std::error::Error = &e;
        while let Some(src) = std::error::Error::source(cur) {
            msg.push_str(&format!(" — caused by: {src}"));
            cur = src;
        }
        msg
    })?;
```

`exec.rs:317`:

```rust
crate::init_pool_async()
    .await
    .map_err(|e| DbError::config("not_configured", format!("db: lazy init failed: {e}")))?;
```

`orchestrator/register_model/mod.rs:117-123`:

```rust
crate::init_pool_async()
    .await
    .map_err(|e| DbError::Configuration {
        code: "lazy_init_failed",
        message: format!("db: lazy init failed: {e}"),
        hint: None,
    })?;
```

```
Why: Two callsites re-classify the same `String` failure with two
     different `.code`s — `"lazy_init_failed"` and `"not_configured"`.
     The SDK's `err.code` branch is the entire point of the typed
     rail; this drift means the same root cause surfaces under two
     different codes depending on which path triggered the lazy
     init. The duplicated source-chain walk in `init_pool_async`
     also re-implements `error::walk_pg_chain`.
Fix: Promote `init_pool_async` to `Result<(), DbError>` using
     `DbError::config_hinted("lazy_init_failed", msg,
     "ensure DATABASE_URL points at a reachable Postgres
     and the cluster is up")`. Both callers then just `?`-flow.
     Removes 16 lines of source-chain walk + the code-name drift in
     one commit. The `hint` field added in f1c5184e is the missing
     piece this enables.
Verification:
  Grep -n 'pub async fn init_pool_async' crates/plugin-db/src/lib.rs
  Grep -n 'lazy_init_failed\|not_configured' crates/plugin-db/src
```

### 5. Lifetimes — new patterns from recent commits

`f1c5184e` added `hint: Option<String>` to `DbError::Configuration`.
All 9 `Configuration` construction sites updated. The new field
propagates by-value through `prefix_message` (which intentionally
leaves Configuration alone — see `error.rs:363-367` for the
contract).

`OrchestratorLockGuard<'p>` lifetime unchanged. Its `release()` method
still takes `mut self` (consume); `into_held()` still consumes. No
new generic-over-lifetime patterns introduced.

`first_row_or_internal<'a, R>(rows: &'a [R], op: &'static str) ->
Result<&'a R, DbError>` — pre-r7. Generic over row type so tests can
exercise without `compio_postgres::Row` construction. Clean.

**No new lifetime patterns; nothing to flag.**

### 6. Idiomatic patterns — match vs if let, ? propagation

R6/R7 fixed the inline `match &mut err { ... }` 8-arm walker in
`migrations.rs::coded_db` (commit `deeefe18`). The function is now a
3-line shim around `error::prefix_message`. Good.

R7's eight inline `prefix_message` sites in `replication.rs` — verified
unchanged at 4 sites (`prefix_message` still inline in 4 spots even
after R7's MIN-R7-1 sweep was supposed to close them). The R7 plan
suggested consolidating them into `coded_sql`. I confirm 4 are still
inline. Probably below the threshold worth a finding (the inline form
is one extra line each), but R7's MIN-R7-1 has NOT shipped.

**[MINOR-R8-3] `v8_bridge::runtime_state` consolidation half-done.**

`v8_bridge.rs:92-97` defines:

```rust
pub(crate) fn runtime_state(scope: &mut v8::PinScope<'_, '_>) -> SharedState {
    scope.get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone()
}
```

But 5 callsites still open-code the same 3-line pattern:

```
crates/plugin-db/src/v8_classes/migration.rs:336
crates/plugin-db/src/v8_classes/migration.rs:606
crates/plugin-db/src/v8_classes/migrations.rs:139
crates/plugin-db/src/orchestrator/auto_tx.rs:49
crates/plugin-db/src/orchestrator/auto_tx.rs:85
```

```
Why: The doc-comment on `runtime_state` claims it's "shared by every
     callback + dispatch helper; consolidated here to avoid copy-
     pasting the expect" — but 5 of ~10 callsites bypass it. The
     pattern works either way, but the consolidator's docstring
     promises the consolidation has happened.
Fix: One sweep: replace the open-coded blocks with `let state =
     v8_bridge::runtime_state(scope);`. 15 lines deleted, the
     consolidator's contract restored.
Verification:
  Grep -n 'get_slot::<SharedState>()' crates/plugin-db/src
  # Should be only the v8_bridge::runtime_state definition site after
  # the sweep.
```

### 7. Resource lifecycle — Drop impls

7 Drop impls:

```
src/read_set.rs:346             Active            — RAII guard for thread-local
src/wal_consumer.rs:157         SuppressGuard     — clear thread-local suppression set
src/v8_classes/migration.rs:84  Migration         — V8 GC finalizer cleanup
src/v8_classes/subscription.rs:55 Subscription   — broker entry GC
src/v8_classes/transaction.rs:100 Transaction    — pending-emit drain on tx-handle drop
src/orchestrator/lock_guard.rs:212 OrchestratorLockGuard — log "leak:" on missed release
src/replication_ops.rs:362      ConsumerRunningGuard — unmark consumer-running
```

`Migration::drop` (`migration.rs:94-131`) wraps `compio::runtime::
spawn` in `std::panic::catch_unwind` because a V8 GC finalizer can
fire during runtime shutdown when spawn would panic. Verified the
`catch_unwind` is `AssertUnwindSafe`-wrapped over a closure that
captures no `&mut` state and the future is `move`-captured. Correct
defensive shape.

`ConsumerRunningGuard::try_claim` (`replication_ops.rs:356-359`) uses
`then(|| Self { app_id })` rather than `then_some(Self { app_id })`
because `then_some` evaluates eagerly. Doc-comment at `:350-355`
explains. **r7's NEW-MINOR catch is correctly fixed and pinned by
lifecycle tests (`consumer_running_guard_*` at `:378-413`).**

**[MAJOR-R8-4] `OrchestratorLockGuard::release()` still flips
`released = true` even when the unlock SQL erred. R7's MAJOR-R5-5
remnant is unchanged.**

`lock_guard.rs:144-183`:

```rust
pub(crate) async fn release(mut self) -> Result<Option<PooledClient<'p>>, DbError> {
    if self.released { return Ok(self.client.take()); }
    if let Some(client) = self.client.as_ref() {
        let unlock_sql = "...";
        if let Err(e) = client.query_text_params(unlock_sql, ...).await {
            tracing::warn!(... "pg_advisory_unlock failed");
        }
    }
    self.released = true;            // ← flips even on Err
    Ok(self.client.take())
}
```

The warn-log gives observability, but:

- `Drop` sees `released = true` and stays silent — the
  "leak:" diagnostic at `:227-237` doesn't fire on a confirmed-failed
  unlock.
- The `Result<Option<PooledClient<'p>>, DbError>` return type
  promises an error rail, but the `Err` arm is unreachable. Either
  the rail is honest (propagate the unlock error) or the type is
  honest (return `Option<PooledClient<'p>>`).

```
Why: A failed `pg_advisory_unlock` means the session-scoped lock is
     held by the connection going back into the pool. The warn-log
     records it once; nothing else surfaces it. The Drop log path
     (which carries the operator-facing "concurrent register_model
     callers for this app will stall" message) is suppressed
     because `released = true`. Net result: observability is by
     log-level only, not by structured state.
Fix: Three options in increasing intrusiveness:
     (a) Don't flip `released`. Then Drop fires the leak log — but
         only on `released = false` paths. Adds duplication
         (warn + drop log) for the same incident; tolerable.
     (b) Tristate: `released_state: { Pending, Confirmed, Failed }`.
         Drop logs leak on `Pending | Failed`, suppressed on
         `Confirmed`. Costs one byte and one enum, gains a clean
         contract.
     (c) Propagate the unlock `Err` via the existing `Result`
         return type so the caller decides. Most invasive (every
         release-site grows an arm) but most honest.
     R7 recommended (b). Still the right call.
Verification: lock_guard.rs:144-183; trace `released =` assignments.
```

**[MINOR-R8-5] `into_held()` is `#[allow(dead_code)]`.**

`lock_guard.rs:196-209`. The doc-comment defends keeping the method:
"flag it `dead_code` until that arrives so the invariant stays
codified at the guard boundary rather than re-discovered as another
open-coded unlock sequence." Reasonable, but the method has 12 lines
of code + 13 lines of comments + a `.expect()` panic surface. Three
options:

- Delete it (1 commit; resurrect from git when the caller actually
  appears).
- Keep it but exhaustively test it (currently only
  `into_held_flips_released_flag` exists, which doesn't actually
  call `into_held()` — see test comments at `:281-291` which
  acknowledge they "mirror the prefix of into_held's body" rather
  than calling it).
- Convert it into an associated function on `RegisterContext` only
  surfaced behind `#[cfg(feature = "future-handoff")]`.

R7 didn't flag this; it's borderline. Calling it minor.

```
Why: Dead code is a contract liability — the `.expect("...into_held
     called on guard with no client")` is a panic surface that
     exists for a caller that doesn't exist. The test that pins the
     invariant fakes the body via comment rather than calling the
     method.
Fix: Delete `into_held` until a real caller appears; the lifecycle
     invariant is already pinned by `release()`'s implementation.
     If a future caller needs hand-off, they re-introduce it with a
     real test that calls it end-to-end.
Verification:
  Grep -n 'into_held' crates/plugin-db/src
  # Only production callsite is the docs in lock_guard.rs itself.
```

### 8. Type ascription — readability of new helpers

`f1c5184e` added `DbError::config_hinted(code: &'static str, message:
impl Into<String>, hint: impl Into<String>) -> Self` alongside the
existing `DbError::config()`. The pair is used at 1 production site
(`wal_consumer.rs:347`); 8 other `Configuration` construction sites
use the struct-literal form `DbError::Configuration { code, message,
hint: None }` rather than the helper.

```
Why: The helper exists so the most common pattern (config error WITH
     a hint) is one call rather than four field-by-field lines. But
     1-of-9 adoption suggests the helper isn't reachable from where
     contributors look. Either bake it as the default constructor
     (force code to use it) or accept the helper is decorative.
Fix: Convert the 8 struct-literal sites to either `DbError::config(...)`
     (when `hint: None`) or `DbError::config_hinted(...)` (when a hint
     is present). Net: zero LOC delta but consistency. The
     `backend/postgres.rs:593-600` "exhausted retry budget" site
     should grow a hint at the same time ("investigate persistent
     CIC failure cause" or similar).
Verification:
  Grep -n 'DbError::Configuration\s*{' crates/plugin-db/src
  Grep -n 'DbError::config\(\|DbError::config_hinted\(' crates/plugin-db/src
```

This is a [MINOR-R8-6].

### Bonus findings (not in original audit dimensions)

**[MINOR-R8-7] `replication.rs:275` builds `format!("{e:#}")` outside
the if-branch that uses it.**

```rust
let is_wal_level_misconfig = e.as_db_error().map(...).unwrap_or(false);
let msg = format!("{e:#}");   // ← unconditional alloc
if is_wal_level_misconfig {
    DbError::Configuration { message: format!("... {msg}"), ... }
} else {
    let mut err = DbError::from_pg(&e);   // builds its own message
    prefix_message(&mut err, "...");
    err
}
```

```
Why: The else-arm uses `DbError::from_pg(&e)` which calls
     `walk_pg_chain(&e)` to build its own message. `msg` is dead in
     the else-arm but still allocated. Cold path so not load-bearing.
Fix: Move `let msg = format!("{e:#}")` inside the `if
     is_wal_level_misconfig` branch.
Verification: replication.rs:268-294
```

**[MINOR-R8-8] `try_mark_consumer_running` allocates `String` even on
the lost-race path.**

`context.rs:433-435`:

```rust
pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
    self.running_consumers.insert(app_id.to_string())
}
```

`HashSet::insert` always takes ownership, so the `to_string()` is
required regardless. But on the lost-race path (the marker already
exists) the new `String` is dropped immediately. A pre-check
(`!self.running_consumers.contains(app_id)`) saves one alloc per
lost race. Lost-race is rare (one consumer per app), so this is
mostly cosmetic.

```
Why: `HashSet<String>` doesn't support borrowed-key insertion, so
     allocating to_string() is necessary on the won-race path but
     wasted on the lost-race path.
Fix:
  pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
      if self.running_consumers.contains(app_id) {
          false
      } else {
          self.running_consumers.insert(app_id.to_string())
      }
  }
  Costs one extra hash on the won path, saves one alloc on the lost
  path. Probably below the noise floor; flag only because the
  existing comment ("close the race between dispatch's idempotent
  gate and the task's first poll") implies this gets exercised
  enough to care.
Verification: context.rs:433-435; benchmark via consumer-spawn loop.
```

**[MINOR-R8-9] `diff.rs` uses `try_get(...).unwrap_or_default()` 13×
on system-catalog queries; inconsistent with the `first_row_or_internal`
pattern.**

`diff.rs:230-330` reads pg_attribute / pg_constraint / pg_index by
column name. Every column is requested by name; the `try_get` returns
`Err` only if the name doesn't exist in the row. The
`unwrap_or_default()` silently produces empty-string defaults for
table/column/pg_type — if the introspection SQL drifted (column
renamed) the diff would see "table '' with column ''" entries and
match nothing in the user's declared schema, producing a destructive
change set on next deploy.

`error::first_row_or_internal` was extracted specifically to close the
analogous bug class (silent empty-RETURNING → audit-id-0). The
per-column `unwrap_or_default()` is the same bug class one layer down.

```
Why: System-catalog columns are queried by name; a typo or rename in
     the introspection SQL silently produces empty-string columns
     which then mis-diff against the declared schema. The same bug
     class `first_row_or_internal` closes at the row level.
Fix: Promote the inner loop to fail-fast: `let table: String =
     row.try_get("table_name").map_err(|e| DbError::Internal {
         message: format!("diff: read columns: missing table_name: {e}")
     })?;`. Or extract a helper `required_col!(row, "table_name",
     String)` since the pattern repeats 13×.
Verification: diff.rs:230-330 — count of `try_get(...).unwrap_or_default()`.
```

**[MINOR-R8-10] `auth/session.rs:134,234` — `clone().unwrap_or_default()`
on Option<String>.**

`init.actor_id.clone().unwrap_or_default()` allocates an empty String
when `actor_id` is `None`. `as_deref().unwrap_or("")` would let `&str`
flow into the params slice without alloc. Hot path? No — session mint
is per-connection-acquire. Cosmetic.

```
Why: Unnecessary String allocation on the None arm of an Option<String>.
Fix: `init.actor_id.as_deref().unwrap_or("")`.
Verification: auth/session.rs:134, :234
```

---

## SQLSTATE rail audit (post-`f6043126`)

The structural SQLSTATE-vs-substring sweep is now end-to-end:

| Site | Pre-r7 | Current |
|---|---|---|
| `auth/session.rs::classify_p0001_detail` | substring | `as_db_error()?.code() == &SqlState::RAISE_EXCEPTION` |
| `replication.rs:208-225` (DUPLICATE_OBJECT) | `msg.contains("42710")` | `as_db_error()?.code() == &SqlState::DUPLICATE_OBJECT` |
| `replication.rs:260-294` (wal_level) | `msg.contains("55000")` ‖ `msg.to_lowercase().contains("wal_level")` | `as_db_error()?.code() == &SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE` |
| `error.rs::from_pg` | always SqlState | unchanged |
| `backend/postgres.rs::create_index_with_recovery_audited` | always SqlState | unchanged |

`Grep -nE 'contains\("[0-9P]{5}"\)' crates/plugin-db/src` returns 0
hits. **Rail complete.**

The `classify_detail_token` extraction in `f6043126` adds 7 unit
tests pinning the SDK-contract surface (the 5 P0001 DETAIL codes +
unknown-token-returns-None + distinctness). The latter is genuinely
nice — protects against a future copy-paste collapsing two codes to
the same string.

---

## Recommended commit ordering (3 commits, then stop)

1. **`init_pool_async` → `Result<_, DbError>`** ([MAJOR-R8-2]). Closes
   the code-name drift, removes the source-chain walk duplication,
   makes the `hint` field useful at this site, and shrinks the
   `Result<_, String>` count by 1. ~30 LOC delta.

2. **`OrchestratorLockGuard::release` tristate** ([MAJOR-R8-4],
   carried over from r7's MAJOR-R5-5 remnant). Tristate
   `released_state` gives Drop a hook into the failed-unlock case;
   the `Result<_, DbError>` return type stays but starts being
   honest. ~25 LOC delta.

3. **`error.rs` preamble drift** ([MAJOR-R8-1]). Update the preamble
   to enumerate the 3-site V8-parser exception category alongside
   the existing validate/hex pair. ~10 LOC delta.

Everything else (MIN-R8-3..10) is decorative — leave them open or
sweep them all in one commit at a future cycle if 95+ is the bar.

---

## Score

**94/100** (+1 vs r7's 93).

### Score breakdown

| Dimension | r7 | r8 | Notes |
|---|---|---|---|
| Correctness | 95 | 95 | No new correctness gaps; the SQLSTATE rail is now complete. The `release()` unlock-confirmation gap (R5-5 remnant) is the lone open question. |
| Performance | 90 | 90 | No regressions. MIN-R8-7 / 8 / 10 are all rare-path allocs. |
| Security | 95 | 95 | classify_p0001_detail extraction + tests pin the SDK refusal contract. No new attack surface. |
| API Design | 95 | 95 | The `hint` field is well-placed; `config_hinted` adoption is patchy (MIN-R8-6) but not load-bearing. |
| Rust Idioms | 93 | 96 | `coded_db` collapse onto `prefix_message` + structural SQLSTATE checks raise this materially; the `Result<_, String>` preamble drift drags it back from 97 to 96. |

### Comparison vs r7

| | R7 (93) | R8 (94) | Delta |
|---|---|---|---|
| CRITICAL | 0 | 0 | — |
| MAJOR open | 1 (R5-5 remnant) | 3 (R5-5 still, R8-1 preamble, R8-2 init_pool) | +2 *(R7's MIN-R7-1/R6-1 carry-overs cleared; new doc-drift surfaced under fresh audit)* |
| MAJOR closed since prior round | 3 (R5-1, R5-4, R6-1) | 2 (R5-1 extended to replication.rs; coded_sql/coded_db full consolidation) | +2 net |
| MINOR open | 8 | 8 | — *(churn: 3 R7 minors closed by f6043126/deeefe18; 5 new MIN-R8-3..10 surfaced)* |
| Structural / contract tests | 6 | 13 | +7 *(classify_detail_*, classify_detail_codes_are_distinct, plus signature guards for keys/session/bootstrap)* |
| `contains("[5-digit-state]")` substring matches | 2 | 0 | -2 *(rail complete)* |
| `Result<_, String>` undocumented sites | 0 | 4 | +4 *(preamble drift)* |
| Lines of duplicated `coded_sql` body across modules | 5 wrappers + 4 inline | 5 wrappers, 0 inline | -4 *(deeefe18)* |

### Where is further investment counterproductive?

The crate is at 94/100 with 392 unit/integration tests, zero
production unwraps outside V8-OOM idiom, complete SQLSTATE-typed
classification, and a well-curated unsafe surface with non-pro-forma
SAFETY comments.

**Score 95** is reachable in 1–2 commits (the three MAJOR items
above). The R8 MAJORs are doc-drift + a known-since-r7 design
question (tristate `released_state`) — non-systemic.

**Score 96** would require closing the residual decorative items
(MIN-R8-3 runtime_state consolidation, MIN-R8-6 helper adoption,
MIN-R8-9 catalog-column `try_get` discipline). 30–40 LOC across 5
files; mostly mechanical.

**Score 97+** would require either: substantial new test coverage
for the remaining untested paths (the `Configuration { hint }` round-
trip into JS Error properties; the auto-tx retry rail); OR an
architectural change like splitting `migrations.rs` (which is large
and busy) from `migration.rs` further. Neither is a code-quality
gap; both are scope expansion.

**My honest call: stop at 95. The three MAJOR items above are
genuinely worth fixing because they affect SDK-facing contracts
(code drift) or operator-facing observability (lock leak). Beyond
that the marginal LOC-per-quality-point ratio inverts — the crate
has reached the "polish > rewrite" regime, and the next cycle's
audit-finder will produce findings indistinguishable from style
preference.**

---

## Verification commands (regression baseline)

```sh
# RefCell-across-await
Grep -nE '\.borrow(_mut)?\(\)' crates/plugin-db/src

# Unsafe count — expect 7 (or audit any new occurrence)
Grep -n '\bunsafe\b' crates/plugin-db/src

# Production panic surfaces — every hit should be V8-OOM idiom OR
# guarded-arm precondition. New unwraps in non-V8 code = regression.
Grep -nE '^\s*[^/].*\.unwrap\(\)' crates/plugin-db/src | grep -v '#\[cfg(test)\]'

# Result<_, String> — expect 7 (validate + hex pair + init_pool +
# 3 V8 parsers). If a new site appears, decide whether to document
# or promote.
Grep -nE 'Result<[^,>]+,\s*String\s*>' crates/plugin-db/src

# Drop impls — expect 7. Each should have a doc-comment explaining
# its lifecycle role.
Grep -nE '^impl.*Drop\s+for' crates/plugin-db/src

# SQLSTATE substring matches — expect 0. Any reappearance is a
# regression of the f6043126 work.
Grep -nE 'contains\("[0-9P]{5}"\)' crates/plugin-db/src

# `released = true` writes in lock_guard — expect 4 (acquire init :125,
# release :181, into_held :198, test :289). If a fifth appears, audit
# whether it's the tristate fix (R8-4) or a regression.
Grep -n 'released = true\|released: true' crates/plugin-db/src/orchestrator/lock_guard.rs

# Configuration hint adoption — config_hinted callsites + struct-literal
# callsites should converge over time.
Grep -n 'DbError::config_hinted\|DbError::Configuration\s*{' crates/plugin-db/src

# get_slot::<SharedState>() open-coding — expect 1 (the v8_bridge
# definition). 5 callsites currently bypass; if it stays 5, accept
# the duplication as documented; if it drops to 1, MIN-R8-3 closed.
Grep -n 'get_slot::<SharedState>()' crates/plugin-db/src
```
