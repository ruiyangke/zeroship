# plugin-db code critique — 2026-05-22 R3

**Score trajectory: 78 (R1) → 85 (R2) → 87 (R3)**

Scope: `crates/plugin-db/` at HEAD `309ed52f`.
Lens: Rust correctness and idioms only. Architecture / security live in
sibling reviews.

The three commits called out in the brief verified clean (see "Verified
recent commits" below); progress on the open R2 items is mixed — two of
three carried over (release_advisory_lock void trait, v8_bridge unwrap)
and one is moot (broker prune-on-empty).

---

## Verified recent commits

### `309ed52f` — v8_classes/replication.rs + db.rs cross-app appId removal

Verified clean. The new `resolve_setup_app_id` (replication.rs:107) and
`resolve_consumer_app_id` (db.rs:323) are pure functions, take borrowed
inputs, return owned `String`, and carry comment-as-invariant ("INVARIANT:
never read app-id-shaped fields from `_opts`") plus unit tests that
double as regression guards. No new panic sites, no new RefCell-across-
await borrows. The pattern is a model the rest of the v8_classes should
adopt for security-critical resolution policies.

One nit (M-NEW-4 below): the `_scope` and `_opts` parameters on
`resolve_consumer_app_id` exist purely to mirror the v8_method
signature. The function never reads them; the comments justify it as
"future fields can be plumbed through". A `#[allow(unused_variables)]`
on the underscored params would make the deliberate-no-op explicit; as
written rustc emits `unused_variables` warnings that the underscore
prefix silently suppresses. Fine, but the cost is an inconsistency check
the compiler can't run.

### `ed697c45` — migrations.rs `map_audit_bootstrap_err`

Verified clean. The four `audit_bootstrap_failed`-shaped sites
(`exec_begin`, `exec_status`, `exec_cancel`, `exec_reset`) now route
through one helper at `migrations.rs:117`. The helper preserves
SQLSTATE-classified variants (`Transient`, `LockContention`, …) verbatim
and only re-wraps the catch-all `Internal` arm with the operator-facing
prefix. Three unit tests cover the canonical retryable / lock-contention
/ catch-all paths.

Minor: the doc comment block above `coded_db` at lines 78-81 begins
with a stranded `///` block ("SQL-error helper — classify the Postgres
error...") that doesn't belong to any item — it's an orphaned doc from
the pre-deletion `coded_sql` helper. Cosmetic.

### `3bb41fa1` — bootstrap.rs lock-leak fix

Verified clean. The pattern mirrors apply.rs Pass-1 exactly: capture
the post-acquire body in an async block, then on `Err` issue
`pg_advisory_unlock` via the same `lock_client` BEFORE dropping it. The
unlock SQL (`bootstrap.rs:179-182`) uses the same `key` value the
acquire used (constructed via `lock_key(app_id)`), and the
`lock_key_formats_with_zs_reg_prefix` test pins the format contract.

The unit-test commentary at `bootstrap.rs:191-216` explicitly
documents why an in-crate test isn't viable (the function is
parameterised over a concrete `&PostgresBackend`, so any test needs a
live pool — at which point it's an integration test). A future refactor
that lifts `bootstrap` onto the `Backend` trait would let a fake-backend
test exercise the error-path unlock; the doc-comment flags that
explicitly. Reasonable hand-off.

---

## R2 carry-overs status

### CLOSED in R3

- **R2 I-NEW-1** (`insert_backfill_running` id=0 silent failure) —
  `audit.rs:613-618` now returns `DbError::Internal` on empty
  RETURNING. Regression test at `audit.rs:880-894`.

- **R2 I-NEW-3** (hand-rolled JSON in
  `create_index_with_recovery_audited`) — `backend/postgres.rs:417`
  routes the envelope through `serde_json::to_string` with a typed
  fallback. The escaping hazard is gone.

- **R2 M-NEW-3** (broker prune-on-empty timing) — re-audit shows this
  is moot. The publish loop at `broker.rs:457-462` calls
  `s.push(...)`, which never closes a subscription; closure happens
  only via external `close()` / `Subscription::Drop`. The next
  `publish` prunes the freshly-emptied bucket on its `retain` pass.
  Behaviour is correct; the R2 finding overstated the risk.

### STILL OPEN

- **R2 I-NEW-2** (`release_advisory_lock` trait still void) —
  `backend/mod.rs:154` still returns `()`. The justification comment at
  `mod.rs:150-152` claims "Always succeeds at the trait level — call
  sites already swallow the underlying error", which is at best
  half-true: the failure mode is *invisible* at the trait boundary, not
  *absent*. See I1 below.

- **R2 M-NEW-2** (`v8_bridge.rs:217` array `try_into().unwrap()`) —
  unchanged. See M1 below.

---

## New findings

### IMPORTANT

**I1. Trait `release_advisory_lock` still returns `()`** — `backend/mod.rs:154`

```rust
async fn release_advisory_lock(&self, client: &Self::Client, key1: &str, key2: &str);
```

The trait commits to "this call cannot fail at the type level", but
`pg_advisory_unlock(...)` can fail under network errors, connection
resets, and the wrong-backend-session case. Every call site has worked
around the trait by issuing the unlock SQL *inline* (apply.rs:206-208,
bootstrap.rs:181-183, run_pipeline/mod.rs:222-224) rather than going
through the trait method. The trait method's only consumer is
`migrations.rs:288, 624`.

This is now the second R3 review where the void return is flagged; the
inline-unlock pattern is the de facto interface and the trait method
should follow. Either:

```rust
// Promote to fallible
async fn release_advisory_lock(...) -> Result<(), DbError>;

// Or remove the trait method entirely and inline-unlock everywhere
```

Today the trait method has two production call sites, both in
`migrations.rs`. The `exec_commit_batch` path at line 624 ignores the
failure on the terminal-write happy path; the `exec_begin` path at line
288 fires only when an audit-row precondition rejects the run (the run
hasn't started). Risk is bounded but the type signature still says "no
return value" — and the four other unlock sites *all* chose to bypass
the trait method, which is a structural code smell.

Fix: promote to `Result<(), DbError>`. Both `migrations.rs` call sites
can `let _ =` if they want; the trait shouldn't make the choice for
them. Carried over from R1-I6 and R2-I-NEW-2.

---

**I2. Many helpers in `replication.rs` / `auth/*` still return `Result<_, String>`** — `replication.rs:81,97,102,147,314,411,496`; `auth/bootstrap.rs:52,169,187,225,272,316,347,414,494,599,630,696,965`; `auth/keys.rs:54,86,113`; `auth/session.rs:75,152,206,219,314,328`; `diff.rs:184,355,380`; `lib.rs:347`; `exec.rs:287`; `v8_classes/migration.rs:454,742`; `v8_classes/migrations.rs:221`; `orchestrator/register_model/validate.rs:57`.

R2 claimed "`Result<_, String>` fully eliminated from production code".
Grep returns ~50 production-rail sites; the largest concentration is
`replication.rs` (7 sites, including the entire watchdog / setup /
drop-abandoned surface) and `auth/bootstrap.rs` (~15 sites). These are
the LAST big rails on string-typed errors, and:

- `replication.rs` failures route through `replication_ops.rs:84` as
  `DbError::Internal { message: e }`, which assigns `.code = "internal"`
  uniformly. A `wal_level != logical` config error (replication.rs:222)
  and a transient connection drop (replication.rs:182) both surface
  with the same `.code` — the SDK cannot distinguish them.
- `replication.rs:147` `ensure_publication_and_slot` is the canonical
  caller-visible entry, exercised by `Replication::setup` and
  `Db::startReplicationConsumer`. Both of those v8_classes now have
  tight unit-test coverage on the security path; the error-rail
  typing is the next-biggest UX gap.
- `validate.rs:57` is documented and intentional (the envelope is the
  SDK wire contract; the `SchemaRefused` wrapper at `mod.rs:204-208`
  preserves it). Carve-out is fine.

Fix: convert `replication.rs` first. The seven functions all map to
specific `DbError` variants:

| Function | Currently returns | Should return |
|---|---|---|
| `sanitise_app_id` | `Result<String, String>` | `Result<String, DbError::ValidationFailed>` |
| `ensure_publication_and_slot` | `Result<SetupOutcome, String>` | `Result<SetupOutcome, DbError>` (Transient on conn, Configuration on `wal_level`) |
| `watchdog_query` | `Result<Vec<SlotHealth>, String>` | `Result<_, DbError>` |
| `drop_abandoned_slots` | `Result<Vec<String>, String>` | `Result<_, DbError>` |
| `slot_status` | `Result<Option<Value>, String>` | `Result<_, DbError>` |

Then `replication_ops.rs:84,118,155,215` flow through `e.to_op_error()`
verbatim, the SDK gets `.code = "configuration"` /
`"transient"` discrimination, and the R1 M1 ledger finally closes.
Estimated: similar shape to commit `b94fbdeb` which did the same for
`register_model`.

Phase-2: `auth/bootstrap.rs`. Fifteen helpers all return
`Result<_, String>`; the bootstrap is called once at module init so
hot-path cost is zero, but the error UX is the same problem.

---

**I3. `replication.rs:236` empty-RETURNING returns `lsn = ""` silently** — `replication.rs:233-237`

```rust
lsn = rows
    .first()
    .map(|r| r.get::<_, String>("lsn"))
    .unwrap_or_default();
```

Direct parallel to the R2 I-NEW-1 fix in `audit.rs:613-618` —
`pg_create_logical_replication_slot()` should always RETURN one row;
empty means RLS bypass or a trigger ate the result. R2 fixed the audit
helper; the replication helper has the same shape and is still on the
silent `unwrap_or_default()` path. The empty LSN string then flows back
to JS as `confirmedFlushLsn: ""`, which `WalConsumer::new(...).
with_start_lsn("")` accepts and then passes to `START_REPLICATION SLOT
... LOGICAL 0/0` (since `""` doesn't parse — Postgres treats unset as
"resume from slot's confirmed_flush_lsn", which is also `""`, so the
consumer may start at WAL head, silently dropping any pending events).

Fix: mirror R2 I-NEW-1.

```rust
lsn = rows
    .first()
    .map(|r| r.get::<_, String>("lsn"))
    .ok_or_else(|| "replication: CREATE SLOT returned no row".to_string())?;
```

---

**I4. `auto_tx.rs` uses legacy `OpResult::Failed { error: String }` path** — `orchestrator/auto_tx.rs:67-71, 104-108`

```rust
Err(e) => OpResult::Failed {
    op_id,
    error: e.into_string(),    // ← collapses DbError → String
    request_id,
},
```

`e.into_string()` strips the typed `DbError` to a flat message before
crossing the V8 boundary. Compare against
`orchestrator/transaction.rs:93-97` which routes the same path through
`OpResult::JsValue { value: ResolveValue::RejectError(e.to_op_error()) }`
— preserving SQLSTATE classification + `.code` for the SDK.

The reason is structural: `auto_tx` calls `setup_promise` (the
op-id-based legacy path) instead of `setup_js_promise` (the
PromiseResolver-Global path). Migrating it would require coordinating
with `runtime/src/core/state.rs:818`'s `OpResult::Failed` consumer.

Today's symptom: `__zsBeginAutoTx` and `__zsEndAutoTx` rejections reach
the JS SSR shim as a bare `Error("db: ...")` with no `.code` — the SDK's
retry-on-`transient` logic can't fire because the discriminator is
gone. Production rarely hits this (auto-tx failures are typically
catastrophic: connect refused, pool exhausted) but the asymmetry with
explicit-tx is real.

Fix: route through `setup_js_promise` + `OpResult::JsValue {
ResolveValue::RejectError(e.to_op_error()) }`, same as
`orchestrator/transaction.rs`.

---

### MINOR

**M1. `v8_bridge.rs:217` array `try_into().unwrap()` (R2 M-NEW-2 carry-over)**

```rust
if v.is_array() {
    let arr: v8::Local<v8::Array> = v.try_into().unwrap();
```

R2 flagged the same pattern; not addressed in R3. Should be:

```rust
let Ok(arr): Result<v8::Local<v8::Array>, _> = v.try_into() else {
    return Value::Null;
};
```

Carried.

---

**M2. `v8_bridge.rs:170-175` boundary check allows 2^63 to coerce to `i64::MAX`** — `v8_bridge.rs:169-180`

```rust
if v.is_number() {
    let n = v.number_value(scope).unwrap_or(0.0);
    if n.fract() == 0.0 && n >= i64::MIN as f64 && n <= i64::MAX as f64 {
        let i = n as i64;
        if (i as f64) == n {
            return Value::Number(serde_json::Number::from(i));
        }
    }
    ...
}
```

Same off-by-one as `v8_classes/migration.rs:checked_int` (lines 289)
which was tested and fixed. Here it's still open:

- `i64::MAX as f64` rounds UP to `9_223_372_036_854_775_808.0_f64`
  (= 2^63, one past `i64::MAX`).
- A JS value of exactly `9_223_372_036_854_775_808.0` then passes the
  `<= i64::MAX as f64` test.
- `n as i64` saturates to `i64::MAX` (rust's `as` for OOB float→int is
  defined behaviour: clamp).
- `(i as f64) == n` is `i64::MAX as f64 == 2^63 as f64` — and BOTH
  sides round to the same `2^63.0` f64, so the equality check passes.
- The function returns `Number::from(i64::MAX)` for what was actually
  `2^63` on the JS side. Silent precision loss.

The fix lifted into `checked_int` (`>= 2^63.0` for the upper bound)
should be replicated here, or this branch should fall through to the
`from_f64` path on values at the edge.

---

**M3. `wal_consumer.rs:738-739` `.as_millis() as u64` truncation**

```rust
ran_for_ms = ran_for.as_millis() as u64,
backoff_ms = backoff.as_millis() as u64,
```

`Duration::as_millis()` returns `u128`. For durations < 2^64 ms
(584M years) this is fine in practice, but rust's `as` cast on `u128 ->
u64` is a *truncation*, not a check. A `try_into().unwrap_or(u64::MAX)`
would be the typed expression of the same intent and would surface a
violated invariant as a panic instead of a silently-wrong log value.
Cosmetic — log lines, not control flow.

---

**M4. `replication.rs:239` `slot_row[0]` direct index** — `replication.rs:239`

```rust
} else {
    lsn = slot_row[0].get::<_, String>("confirmed_flush_lsn");
    created = false;
}
```

The `else` is reached only when `!slot_row.is_empty()` (line 204), so
the index is safe. But the pattern is brittle: a future edit that
removes the `.is_empty()` guard at the top introduces a panic at
`[0]`. Idiomatic:

```rust
lsn = slot_row
    .first()
    .map(|r| r.get::<_, String>("confirmed_flush_lsn"))
    .ok_or_else(|| "replication: probe returned no rows".to_string())?;
```

Symmetric with M3 / I3 — the pattern's already in use elsewhere in the
file.

---

**M5. `migrations.rs:619` `finalise_backfill` failure silently dropped** — `migrations.rs:619-621`

```rust
let _ = backend
    .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
    .await;
```

The terminal write (driving the audit row to `applied` / `failed`)
fires after the batch committed, so the run itself is durable — but a
silent finalise failure leaves the audit row in `running`, which the
B1 supervisor's recovery sweep will then "recover" (re-issue cancel)
on next startup. Should `tracing::warn!` at minimum so operators can
distinguish "audit row stuck in running because the worker crashed"
from "audit row stuck in running because finalise raced a
serialisation failure".

---

**M6. `migrations.rs:258` `e.into_string()` on `acquire_dedicated_client` failure** — `migrations.rs:255-258`

```rust
let client = backend
    .acquire_dedicated_client()
    .await
    .map_err(|e| coded("tx_connect_failed", &e.into_string(), None))?;
```

`acquire_dedicated_client` returns `DbError::Transient` on connect
failure (`backend/postgres.rs:73`). The `into_string()` here strips
the `Transient` variant and re-stamps it as `Coded { code:
"tx_connect_failed" }`. The SDK then sees `.code = "tx_connect_failed"`
instead of `.code = "transient"` and can't apply the standard transient-
retry policy.

Compare with `coded_db("create schema", e)` at `migrations.rs:249`,
which preserves the typed variant via the `coded_db` helper. The
`tx_connect_failed` site predates the typed rail — it's the lone
outlier in this file.

Fix: replace with `coded_db("dedicated client acquire", e)`. The SDK
then sees `.code = "transient"` plus the operator-facing prefix in the
message.

---

**M7. `wal_consumer.rs:683-688` substring match against error message text** — `wal_consumer.rs:672-690`

```rust
let lc = s.to_ascii_lowercase();
lc.contains("58p01")
    || lc.contains("does not exist")
        && (lc.contains("replication slot") || lc.contains("publication"))
    || lc.contains("invalid slot name")
```

`is_fatal` makes a "do not retry" decision based on substring matching
against the formatted error message. This is fragile: a future
`compio_postgres` version that reformats the error chain (e.g. drops
the "replication slot" prefix in favour of the slot name) would
silently flip the supervisor from "graceful exit on slot drop" to
"hammer reconnect forever". The SQLSTATE check (58P01) is sound; the
two message-text checks are not.

The slot-doesn't-exist case maps to SQLSTATE 58P01 (undefined_object)
on every Postgres version since 13. The "invalid slot name" check is
ostensibly for a client-side validation that doesn't carry a SQLSTATE
— but `WalConsumer::new()` already validates via
`replication::slot_name()` before `run()` is called, so by the time
`is_fatal` runs the name is already known-good. That branch may be
dead code.

Fix: drop the message-text branches; rely on SQLSTATE only.

---

**M8. R1 M7 (`validate_app_id` duplication) status update**

R1 flagged that `audit.rs:741` duplicated logic from `query.rs:61`.
Re-audit: the two functions check *different* things —
`audit.rs::validate_app_id` accepts `[A-Za-z0-9_-]` (allowing hyphens
for app IDs like `app-prod`), while `query.rs::validate_collection`
blocks `pg_*` / `__zeroship*` / null bytes / length / etc. They've
diverged enough that they're no longer pure duplicates — but the
question of "what charset can an app_id contain?" now has TWO
answers in this crate (`audit.rs:830-833` and
`replication.rs:84-92`'s `sanitise_app_id`). The latter rejects hyphens
because Postgres slot names can't contain them; the former accepts
hyphens.

This isn't a bug today (the audit `app_id` is interpolated into a
quoted identifier where hyphens are fine; the replication path
explicitly lowercases + restricts). But two separately-maintained
"valid app_id" predicates with subtly different rules invite drift.
A single `validate_app_id` returning a parsed-and-normalised form
(quoted-identifier-safe for audit, lowercased-no-hyphens for
replication) would make the dependency on each consumer explicit.

Carried open from R1 M7 — same recommendation.

---

## RefCell-across-await sweep (focus dim 1)

Methodology: grep for `\.borrow(_mut)?\(\)` followed within the same
function by `\.await`, walked `broker.rs`, `replication.rs`,
`wal_consumer.rs`, `context.rs`, every file under `v8_classes/`.

**Result: zero RefCell-across-await sites in production code.**

The 14 hits in the multi-line grep are all variants of one pattern:

```rust
state.borrow_mut().spawned_ops.push(Box::pin(async move {
    ... .await ...
}));
```

— the borrow is released by the `.push(...)`'s argument evaluation; the
`.await` lives INSIDE the boxed future, which is `move`'d onto
`spawned_ops`. The borrow is dropped before the future is polled. Sound.

`broker.rs`'s `Subscription::pop` / `push` / `register_waker` all open
a fresh `inner.borrow_mut()` inside the function body and never hold it
across a yield point. The `accepts()` method opens `inner.borrow()` and
walks the read-set synchronously. `next()` at
`v8_classes/subscription.rs:96-126` is the V8 path most at risk of
this footgun — it `.clone()`s the `BrokerSubscription` *before* the
`poll_fn` await so the RefCell borrow doesn't survive the yield.
Defensive comment at line 97-99 documents the rule. Clean.

The R1 finding here ("Stage 8d/8e meaningfully cleaned the crate; zero
RefCell-across-await sites survive") still holds in R3.

---

## Unsafe sweep (focus dim 2)

10 `unsafe` blocks across production code (excluding test modules):

| File | Lines | Justification |
|---|---|---|
| `v8_classes/db.rs` | 402-404 | `Box::from_raw` in V8 Weak finalizer. SAFETY comment at 392-398 names the Box origin + the lifetime constraint (V8 reclaims the wrapper exactly once). Sound. |
| `v8_classes/collection.rs` | 386-388 | Same finalizer pattern. SAFETY comment minimal but the pattern is identical to db.rs. |
| `v8_classes/transaction.rs` | 329-331 | Same finalizer pattern. SAFETY comment at 321-325 names the Drop impl that auto-rollbacks. |
| `v8_classes/subscription.rs` | 201-203 | Same finalizer pattern. SAFETY comment at 194-197 names the broker close. |
| `v8_classes/migration.rs` | 578-580 | Same finalizer pattern. SAFETY comment at 571-574 documents the Drop-spawn cancel. |
| `v8_classes/migration.rs` | 426 | `&*(this_addr as *const Migration)` inside the spawned future. SAFETY comment at 421-425 names the wrapper-Global keepalive. Sound. |
| `v8_classes/migration.rs` | 689 | `&*(raw_addr as *const Migration)` inside the spawned future. SAFETY comment at 679-687 names the keepalive. Sound. |
| `v8_classes/migrations.rs` | 282-284 | Same finalizer pattern. SAFETY comment minimal. |
| `v8_classes/replication.rs` | 148-150 | Same finalizer pattern. SAFETY comment absent. |
| `v8_classes/subscription.rs` | (covered above) | |

Findings:

- 7 of 10 sites are the identical `Box::into_raw` + Weak finalizer
  template. R1-I3 flagged that this should be factored into one
  `install_wrapper<T>(scope, obj, state: Box<T>)` helper; the
  duplication is unchanged in R3. Not a correctness bug — every site
  is sound — but a regression that lands in one site (forgetting the
  `std::mem::forget(weak)` step) would be invisible from the other
  six.

- `v8_classes/replication.rs:148-150` lacks the `// SAFETY:` comment
  the other 6 sites carry. Quote of the pattern at db.rs:392-398 (the
  most-documented site) and applying it here is one paragraph of
  comment. The audit pass at R2 noted "all have SAFETY comments" —
  this is the one exception.

- The two non-template `unsafe` blocks in `v8_classes/migration.rs`
  (lines 426, 689) are the receiver-pin-across-await pattern; the
  keepalive is documented in the SAFETY comments. Sound.

**Recommendation**: extract the 7-site template into a helper. Net
LOC reduction is ~50; the seven SAFETY comments collapse into one.

---

## Panic-risk sweep (focus dim 3)

| Pattern | Production-rail count | Notes |
|---|---|---|
| `unwrap()` | ~30 | Bulk in `v8_bridge.rs` / `v8_classes/*` — V8 allocation primitives (`v8::PromiseResolver::new`, `v8::String::new`, `v8::Function::new`). R1-I4 unaddressed. The `v8_bridge.rs:217` array `try_into().unwrap()` (R2 M-NEW-2) still open. |
| `expect("RuntimeState not in isolate slot")` | 4 sites | All correct panics for the invariant breach during isolate setup. Re-audit OK. |
| `expect(...)` (other) | 2 | `lib.rs:265` test-only (install_tx_marker_for_tests, gated by `#[cfg(any(test, feature = "test-helpers"))]`); `query.rs:694` `String::from_utf8(...).expect("ALPHABET is ASCII")` is sound. |
| `panic!` | 0 | None in production. |
| `unreachable!` | 4 | `v8_classes/collection.rs:222,269`, `read_set.rs:140`, all in exhaustive matches after the variant was already eliminated. Sound. |
| `[index]` on potentially-empty container | 1 | `replication.rs:239` `slot_row[0]` — guarded by `.is_empty()` check at line 204 but brittle. See M4 above. |
| `as` casts that could truncate | 3 | `wal_consumer.rs:738,739` (`u128 → u64` via `as`, see M3); `v8_bridge.rs:171-175` (`i64::MAX as f64` round-up, see M2). All bounded in practice. |

`v8::PromiseResolver::new(scope).unwrap()` count breakdown:

- `v8_classes/migration.rs:339, 613` (sync method, can't return Result)
- `v8_classes/migrations.rs:143` (sync method)
- `orchestrator/transaction.rs:58, 70` (one in early-reject, one in
  main path)
- `orchestrator/auto_tx.rs` via `setup_promise` indirectly
- `replication_ops.rs` via `setup_js_promise` indirectly
- `crud.rs` via `setup_js_promise` indirectly

R1-I4 suggested a central `try_make_resolver(scope) -> Result<...,
OpError>` helper. Today the synchronous methods can't return
`Result<_, OpError>` because the v8_class proc macro expects them to
return `v8::Local<'s, v8::Value>` directly — the `unwrap` is forced by
the function signature. Migrating would require either (a) widening the
return type via the macro, or (b) routing through a "build a rejected
promise" helper inline. Both are non-trivial; status quo (one panic
per failed allocation) is the pragmatic floor.

`v8::String::new(...).unwrap()` is the same shape: synchronous V8
allocation in a context where the only Out-of-Memory recovery is to
panic the worker. Today's runtime treats OOM as fatal anyway.

---

## Error-handling typing (focus dim 4)

R2 claim: "Result<_, String> fully eliminated from production code".

Re-audit: this is *not* true. ~50 production-rail sites remain, listed
under I2 above. The most-prominent is `replication.rs` (7 functions)
and `auth/bootstrap.rs` (15 functions).

`DbError` itself is well-factored: 11 discrete variants, each carrying
its `.code` discipline through `to_op_error()`. The migration is the
issue, not the design.

`DbError` variants vs. flat strings: the discrete-kind design is the
right one. The flat `Coded { code: String }` arm is a pragmatic escape
hatch for codes that other subsystems (the migration lifecycle) own; the
named variants cover the SQLSTATE classes the SDK branches on directly.

One observation: `Configuration { code: &'static str, message: String }`
takes `&'static str` for the code, while `Coded { code: String, ... }`
takes owned. The asymmetry is intentional (the `Configuration` codes
are a fixed enumeration the plug-in owns; `Coded` carries codes from
sibling crates). Worth a doc-comment on the variant signature.

---

## Lifetimes (focus dim 5)

Walked `orchestrator/register_model/bootstrap.rs` (the `'p` borrow
propagation focal point):

- `bootstrap` returns `(RegisterContext, PooledClient<'p>)`. The `'p`
  threads through `apply::apply<'p, B>` which takes `lock_client:
  PooledClient<'p>` and drops it inline. The two are correctly
  decoupled: `RegisterContext` is a pure value type (no borrows), so
  the orchestrator can move it independently of the pool borrow.

- `RegisterContext` (bootstrap.rs:34-52) has no lifetime params.
  Correct — every field is owned (`String`, `i32`, `Vec<IndexSpec>`).

- `apply<'p, B: Backend>` (apply.rs:37) ties `'p` only to
  `lock_client`. The closure `run_op` borrows `&app_id`, `&deploy_id`,
  `&declared_indexes` from the outer scope (not via `'p`); the trait
  bound on `B: Backend` is sufficient.

Unnecessary lifetime annotations: none found in the orchestrator path.
Elision opportunities: none.

---

## Idiomatic patterns (focus dim 6)

- `?` propagation — used throughout the pipeline. The `DbError ::
  From<pg::Error>` and `DbError :: From<QueryError>` impls
  (`error.rs:329-352`) let dispatch helpers `?`-flow Postgres errors
  directly. Clean.

- `match` vs `if let` — generally appropriate. Two-armed matches use
  `if let` / `let ... else`; many-armed use `match`. No drift found.

- Iterator chains — `crud.rs` and `wal_consumer.rs::tuple_to_map`
  are clean. No unnecessary `.collect::<Vec<_>>()` followed by
  re-iteration.

- `Result::ok` discards — 9 sites under `let _ = backend....await`,
  several of which silently drop typed errors. M5 above flags
  `migrations.rs:619` (finalise_backfill); the other sites are
  best-effort cleanup paths (ROLLBACK, DROP INDEX) where the
  follow-up error wouldn't be actionable.

---

## Resource lifecycle (focus dim 7)

Drop impls audited:

- `v8_classes/subscription.rs::Subscription::drop` — takes from
  `inner` (RefCell-guarded), calls `sub.close()`. Idempotent.
- `v8_classes/transaction.rs::Transaction::drop` — checks token
  against the live TX_TOKEN; takes the client and drops it; clears
  pending emits. Idempotent. Sound.
- `v8_classes/migration.rs::Migration::drop` — `take`s `inner`,
  guards against backend-uninitialised (silent return), guards
  against runtime-shutdown via `catch_unwind`. Sound.
- `wal_consumer.rs::SuppressGuard::drop` — calls
  `unsuppress_app`. Idempotent. Sound.

Missing drop guards / advisory locks:

- `MigrationLock` (`context.rs:48-63`) — the `client` field can be
  dropped while `mig_lock` is still in the slot; no Drop impl on
  `MigrationLock` itself ensures the advisory lock is released.
  The release path runs in `exec_commit_batch` (migrations.rs:624)
  and the worker-shutdown `release_active_lock()`. If `mig_lock` is
  dropped via `clear_mig_lock()` without `release_advisory_lock`
  having been called, the lock auto-releases when the client's
  backend session ends — i.e. when the Client is dropped. Sound but
  implicit; a `Drop` impl on `MigrationLock` that explicitly fires
  `pg_advisory_unlock` (best-effort) would make the invariant
  visible.

- `MIG_LOCK` / `TX_CONN` are thread-locals on
  `IsolateDbContext`. Their Drop fires on isolate teardown, which
  drops the contained `Client` — backend session ends, locks
  release. Sound.

- The `PooledClient<'p>` from `bootstrap::bootstrap` carries the
  advisory lock; its Drop returns the connection to the pool
  WITHOUT releasing the lock. The three call sites (apply.rs,
  bootstrap.rs error path, mod.rs:run_pipeline error path) all
  inline-unlock before drop. Pattern is documented (apply.rs:194-202,
  bootstrap.rs:128-138, mod.rs:174-200) — three copies of the same
  comment block. A `LockedPooledClient` newtype with a `Drop` impl
  would centralise the invariant; today it's a "remember to unlock"
  manual discipline.

---

## Type ascription (focus dim 8)

`dispatch_op<R>` (`crud.rs:59-92`):

```rust
async fn run_op<R, EFut, Resolve>(
    ...
    exec: impl FnOnce(query::BuiltQuery) -> EFut,
    resolve: Resolve,
) -> OpResult
where
    EFut: Future<Output = Result<R, DbError>>,
    Resolve: FnOnce(R) -> ResolveValue,
```

`EFut::Output = Result<R, DbError>` ties `R` to the exec helper's
return type; `Resolve: FnOnce(R) -> ResolveValue` consumes it. The
inferer can chain `R` between them WITHOUT explicit annotation at the
call site — the closures inside `dispatch_find_one`, `dispatch_insert`,
etc. don't carry `|x: Vec<Value>|` annotations and compile.

ONE outlier remains: `crud.rs:487` `|n: i64| { ResolveValue::F64(n as f64) }`
in `dispatch_count`. The `i64` annotation is unnecessary — `exec_count`
returns `Result<i64, DbError>` and the inferer ties `R` to it. The
explicit `i64` is presumably defensive against a future refactor of
`exec_count`. Cosmetic.

R1 I2's "split into run_json_op + run_count_op" suggestion: not
implemented, and the rationale is now weaker — the inferer handles the
single-template path without ceremony at every site. Status: closed by
omission.

---

## Score breakdown

| Dimension | R1 | R2 | R3 | Change |
|---|---|---|---|---|
| Correctness | 72 | 80 | 84 | I-NEW-1 closed (audit.rs:613 typed Internal); I-NEW-3 closed (serde_json envelope); I3 (replication.rs lsn=`""` mirror open); M5 (silent finalise) carried |
| Performance | 84 | 84 | 84 | No regression; `has_subscribers` fast-path landed |
| Security | 88 | 88 | 90 | `309ed52f` removed cross-app appId override w/ unit tests |
| API design | 76 | 82 | 84 | `release_advisory_lock` void still open (I1); `OpResult::Failed` legacy path in auto_tx (I4) |
| Rust idioms | 80 | 84 | 86 | DbError discipline now reaches all 4 audit-bootstrap sites; ~50 `Result<_, String>` sites still need migration (I2) |
| **Overall** | **78** | **85** | **87** | Real progress on typed-error rail + security; trait-void return and v8_bridge unwrap remain |

The R3 ceiling is held below 90 by three items, in priority order:

1. **`release_advisory_lock` void return** (I1) — typed honesty issue;
   the inline-unlock pattern at every call site is now the de facto
   API. Single line of trait signature change unblocks the migration.

2. **`Result<_, String>` in `replication.rs` + `auth/bootstrap.rs`**
   (I2) — the largest remaining string-rail sites; converting
   `replication.rs` first is the highest-leverage move because it's
   the actively-exercised reactive-queries surface.

3. **`v8_bridge.rs:217` array `try_into().unwrap()`** (M1) — single
   line. The `let Ok(arr) ... else { return Value::Null; }` rewrite
   should land.

Items below 86 (the next score band) are all M-class — addressable in
isolation:

- M2 (`v8_bridge.rs:171` i64-bound off-by-one) — symmetric with the
  `checked_int` fix already landed in migration.rs.
- M3 (`as_millis() as u64` truncation) — cosmetic but typed.
- M4 (`slot_row[0]` direct index) — defensive against future edits.
- M5 (silent `finalise_backfill` failure) — `tracing::warn!` is the
  fix.
- M6 (`migrations.rs:258` `into_string()` strips Transient) — one-
  line `coded_db` swap.
- M7 (`wal_consumer.rs::is_fatal` message-text substring matching) —
  drop the two text branches; rely on SQLSTATE only.

The R2 ceiling-blockers that are now closed:

- R2-I-NEW-1 (audit insert silent id=0) — closed via
  `ok_or_else(|| DbError::Internal { ... })` at `audit.rs:613-618`.
- R2-I-NEW-3 (hand-rolled JSON envelope in
  `create_index_with_recovery_audited`) — closed via
  `serde_json::to_string` at `backend/postgres.rs:417`.
- R2-M-NEW-3 (broker prune-on-empty timing) — re-audit shows it was
  moot; no code change required.
