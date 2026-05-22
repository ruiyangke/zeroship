# plugin-db Error-UX Review — 2026-05-22 r6

Scope: `crates/plugin-db/src/` at HEAD (post `cbbc9059`, `51ced4a0`,
`34d209b5`, `f7d0961c`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r5.md` (85 / 100).

Lens: SDK-author error-handling discipline. Every finding evaluates the
JS-visible surface — `e.code`, `e.message`, `e.hint` — and whether the
SDK can branch on it without parsing strings. Operator observability
(tracing fields) is in scope where it overlaps with the error UX rail.

---

## TL;DR (changes since r5)

**Resolved since r5 — verified at HEAD:**

- **r5 §2 LOW (4 duplicated `coded_sql` helpers)** — `cbbc9059`
  collapsed `audit.rs::coded_sql`, `auth/{bootstrap,keys,session}.rs::coded_sql`,
  and `diff.rs::coded_sql` to one-line wrappers that compose their
  module-scoped prefix into `crate::error::coded_sql`. The shared helper
  in `error.rs:357-361` (and its `prefix_message` primitive at
  `error.rs:327-345`) is now the single variant-walker. Five `fn coded_sql`
  declarations remain (one per module + the central) but each non-central
  one is two lines that delegate. Wire shape unchanged; the maintenance
  hazard r5 flagged is closed. Verification:

  ```
  grep -n '^fn coded_sql\|^pub\(crate\) fn coded_sql' \
    crates/plugin-db/src/audit.rs \
    crates/plugin-db/src/auth/{bootstrap,keys,session}.rs \
    crates/plugin-db/src/diff.rs \
    crates/plugin-db/src/error.rs
  # 6 hits; the per-module ones are 2-line `crate::error::coded_sql(...)` wrappers
  ```

- **`finalise_backfill` warn-on-err** — `51ced4a0` replaces the silent
  `let _ = backend.finalise_backfill(...)` with `if let Err(e) =` + a
  structured `tracing::warn!` capturing `app_id`, `audit_id`,
  `terminal`, and `error`. Closes the F1-family discipline regression
  flagged in migration-pipeline r5 M7. Verified at
  `migrations.rs:644-657`.

- **`prefix_message` + `coded_sql` test coverage** — five new unit
  tests live with the helper in `error.rs::tests` (`prefix_message_preserves_variant_and_code`,
  `prefix_message_leaves_structured_variants_alone`,
  `sql_violation_variants_stamp_canonical_codes`,
  `retryable_variants_carry_hint`, plus
  `first_row_or_internal_*`). The variant-preservation contract the
  five duplicates used to assert ad-hoc is now pinned in one place.

- **`error.rs` preamble** — `f7d0961c` rewrote the module preamble to
  reflect the post-[I28] state: `Result<_, String>` is now confined
  to the `validate` stage envelope contract + two ASCII parsers in
  `auth/session.rs`. Documentation is no longer stale.

- **`ConsumerRunningGuard` restructure (`34d209b5`)** — no error-UX
  delta. The structural change (moving the `mark_consumer_running`
  call inside `ConsumerRunningGuard::new`) keeps the Drop unmark
  invariant intact without changing what JS sees.

**Still open from r5 (unchanged):**

- r5 §3 (3 P0001 promotions still use `validation()`, not
  `validation_hinted()`) — Pilot Pick #1 (this cycle) addresses the
  *underlying* substring-matching rail; the hint gap is the next
  layer.
- r5 §6 (SDK `withRetry` default predicate only matches
  `optimistic_lock_failure`) — cross-crate scope. Re-verified at
  `sdks/db/src/with-retry.ts:25-30`.
- r5 §8 (`v8_classes/migration*.rs` `parse_*` reject as plain
  `TypeError` with no `.code`) — re-verified unchanged.
- r5 §9 (`coded_db` in `migrations.rs:82-102` is a sixth copy of the
  variant-walker — same shape as the helpers `cbbc9059` consolidated,
  but takes typed `DbError` instead of `compio_postgres::Error`)
  — re-verified unchanged.

**Net score delta vs r5:** see §10.

---

## 1. Verify r5 closures — all 4 verified clean

### [PASS] r5 §1.1 — `replication.rs` typed errors

Re-verified after `cbbc9059`. The four `replication::*` helpers still
return `Result<_, DbError>`; dispatch sites at `replication_ops.rs`
still route via `e.to_op_error()`. No regression.

```
grep -n 'Result<.*, String>' crates/plugin-db/src/replication.rs   # zero in fn sigs
grep -n 'DbError::Internal { message: e }' crates/plugin-db/src/replication_ops.rs  # zero
```

### [PASS] r5 §1.2 — `coded_db` single prefix

Re-verified clean at `migrations.rs:82-102`. Single `"{context}:
{message}"` prefix; structured variants bypassed.

### [PASS] r5 §1.3 — `validate.rs` SchemaRefused docs

Re-verified accurate at `validate.rs:16-32` (no diff in this window).

### [PASS] r5 §1.4 — `auto_tx` OpResult::JsValue + RejectError

Re-verified at `auto_tx.rs:60-72` (begin) and `auto_tx.rs:97-108`
(end); helpers `begin_to_resolve_value` / `end_to_resolve_value`
unchanged.

### [PASS] r5 §2 — duplicate `coded_sql` helpers consolidated

`cbbc9059` collapses 5 walker bodies onto `crate::error::coded_sql`.
Each per-module wrapper is now two lines that compose the prefix
into the context. The "fragile-if-prefix_message-drifts" hazard r5
flagged is closed by construction.

Verification:
```
grep -B1 -A1 '^fn coded_sql' crates/plugin-db/src/audit.rs \
  crates/plugin-db/src/auth/{bootstrap,keys,session}.rs \
  crates/plugin-db/src/diff.rs
# every site is now: `crate::error::coded_sql(&format!("<module>: {context}"), e)`
```

---

## 2. MAJOR-R5-1 — auth/session.rs P0001 substring matching (still open)

### [MED] `auth/session.rs:204-218` — substring `msg.contains(...)` to dispatch on RAISE text

`auth/session.rs:188-225`:

```rust
.map_err(|e| {
    // Walk the source chain — compio-postgres's top-level
    // Display is "db error"; the SQLSTATE-bearing inner
    // DbError is one source-hop away.
    let mut msg = format!("{e}");
    let mut cur: &dyn std::error::Error = &e;
    while let Some(src) = std::error::Error::source(cur) {
        msg.push_str(" | ");
        msg.push_str(&format!("{src}"));
        cur = src;
    }
    // Promote the structured RAISE messages to typed
    // ValidationFailed variants with stable `.code`s the SDK
    // can branch on. The SECURITY DEFINER function raises
    // SQLSTATE P0001 with these messages — they are
    // user-input-shaped refusals, not server bugs.
    if msg.contains("nonce replay detected") {
        DbError::validation(
            "session_nonce_replay",
            "auth/session: nonce replay detected",
        )
    } else if msg.contains("signature expired") {
        DbError::validation(
            "session_signature_expired",
            "auth/session: signature expired",
        )
    } else if msg.contains("invalid session-init signature") {
        DbError::validation(
            "session_invalid_signature",
            "auth/session: invalid signature",
        )
    } else {
        coded_sql("init_session", e)
    }
})
```

  Why: SDK-author impact — the wire `.code` is correct today
  (integration tests at `tests/integration.rs:3608-3697` pin the
  three codes), but the discriminator is the message body, not the
  SQLSTATE classifier (P0001). A future change to the SECURITY
  DEFINER's `RAISE EXCEPTION` text — e.g. switching `"nonce replay
  detected"` to `"replay of nonce detected"` for clarity — silently
  flips that call from `session_nonce_replay` to `transient` (the
  P0001 catch-all path). The compiler doesn't enforce coupling
  between the SQL function body and this Rust file. The substring
  matching is also tolerant of false positives — if the message
  *body* of a different P0001 from elsewhere happens to contain
  `"signature expired"` text (unlikely but unbounded), it gets
  mis-classified.

  Fix (the better shape): switch the SECURITY DEFINER to
  `RAISE EXCEPTION USING ERRCODE = 'ZS001' /* nonce replay */`
  with a small `ZS001`/`ZS002`/`ZS003` namespace (or
  `RAISE EXCEPTION USING ERRCODE = '45001'` etc. in the
  user-defined SQLSTATE range `45000-45ZZZ` Postgres reserves for
  this purpose). Then this match becomes a `code.as_code().eq("ZS001")`
  lookup on the SQLSTATE classifier — both robust to message
  rewording and machine-checkable.

  ```rust
  // Sketch:
  if let Some(code) = e.code() {
      match code.code() {
          "45001" => return DbError::validation_hinted(
              "session_nonce_replay",
              "auth/session: nonce replay detected",
              "the nonce has been used; mint a fresh token and retry",
          ),
          "45002" => return DbError::validation_hinted(
              "session_signature_expired",
              "auth/session: signature expired",
              "re-mint via mint_session_token; tokens have a 5-min default TTL",
          ),
          "45003" => return DbError::validation_hinted(
              "session_invalid_signature",
              "auth/session: invalid signature",
              "token was minted against a retired key or tampered with; refresh credentials",
          ),
          _ => {}
      }
  }
  coded_sql("init_session", e)
  ```

  Note: this finding rolls together r5 §3 (no hints on the 3 P0001
  promotions) since the SQLSTATE migration is the same edit point.
  Adopting `validation_hinted()` here is the natural first
  production caller of the helper (r5 §3 LOW closed in one
  motion).

  Verification:
  ```
  grep -n 'msg.contains' crates/plugin-db/src/auth/session.rs
  # 3 hits at lines 204, 209, 214 — all in this map_err closure
  grep -rn 'validation_hinted' crates/plugin-db/src/
  # 1 declaration in error.rs:291-301, zero production callers
  ```

---

## 3. MAJOR-R5-4 — WalConsumer::new flattens DbError to String (still open)

### [MED] `wal_consumer.rs:333-336` — `e.to_string()` strips `.code`

`wal_consumer.rs:323-344`:

```rust
impl WalConsumer {
    pub fn new(app_id: &str, db_url: &str) -> Result<Self, ConsumerError> {
        // `slot_name` / `publication_name` now return `DbError` — we
        // flatten through `to_string()` so the consumer's wire surface
        // (a `ConsumerError::NotProvisioned(String)`) stays unchanged.
        // The `DbError`'s `.code` is preserved at the V8 dispatch
        // boundary (`replication_ops::start_replication_consumer_dispatch`)
        // which inspects the inner DbError before this call.
        let slot_name = crate::replication::slot_name(app_id)
            .map_err(|e| ConsumerError::NotProvisioned(e.to_string()))?;
        let publication_name = crate::replication::publication_name(app_id)
            .map_err(|e| ConsumerError::NotProvisioned(e.to_string()))?;
        ...
```

  Why: SDK-author impact — the comment claims `.code` is preserved
  at the dispatch boundary "before this call", which is true on
  the *happy* dispatch path. But `WalConsumer::new` has callers
  outside dispatch (see `wal_consumer.rs:913` test, plus future
  reuse): any non-dispatch caller that hits an `invalid_app_id`
  refusal sees `ConsumerError::NotProvisioned(s)` whose `s` is the
  flat message — no machine-readable code. The `ConsumerError`
  enum *itself* is the lossy boundary, not this call site.

  Fix (the right shape):
  - Add a `ConsumerError::CodedNotProvisioned { code: String,
    message: String }` variant carrying the DbError code.
  - At dispatch (`replication_ops::start_replication_consumer_dispatch`)
    keep the existing `to_op_error()` route by mapping the new
    variant via a thin `impl From<ConsumerError> for DbError`
    that preserves code through `DbError::Coded`.
  - Bridge the old `NotProvisioned(String)` to the new variant
    for back-compat; existing match arms keep working.

  Alternative (simpler but lossier): give `ConsumerError` an
  optional `.code()` accessor returning `Option<&str>` so callers
  can read it without changing variants. Less type-safe than the
  variant split.

  Verification:
  ```
  grep -n 'ConsumerError::NotProvisioned' crates/plugin-db/src/
  # 6 hits — Display impl, two map_err's in WalConsumer::new, one
  # is_fatal arm, one test, plus a pattern-match
  grep -n 'pub enum ConsumerError' crates/plugin-db/src/wal_consumer.rs
  # 1 hit at line 236
  ```

  Note: this is a pre-existing fragility — `ConsumerError` was
  designed as a string-based enum before the [I28] sweep raised
  the surface to typed. The flattening is a vestige of the old
  contract.

---

## 4. N6-M10 — coded_sql wrapper chain has 3 allocs per SQL-error path

### [LOW/COSMETIC] Three transient `String` allocs per SQL-error

For each call like `audit::coded_sql("INSERT migrations", e)`:

1. **Wrapper** at `audit.rs:59`:
   `format!("audit: {context}")` — 1 alloc.
2. **Central helper** at `error.rs:359`:
   `format!("{context}: ")` — 1 alloc (the wrapper's string is
   passed by `&str`, not consumed).
3. **`prefix_message`** at `error.rs:337`:
   `format!("{prefix}{message}")` — 1 alloc (replaces the message
   body).

Total: 3 transient `String` allocations per SQL-error path. The
message body itself was already allocated by `walk_pg_chain`.

  Why: SDK-author impact: none. This is a cold-path cost (only
  fires on SQL errors). Operator-side cost is negligible — error
  paths are not throughput-critical. Calling this out as cosmetic
  because the perf review (r6 N6-M10) flagged it; it isn't an
  error-UX defect.

  Fix (if optimised): give `prefix_message` a two-arg signature
  that takes `(module: &str, context: &str)` and writes
  `format!("{module}: {context}: {message}")` directly. Saves
  the wrapper's intermediate allocation. ~15 LOC change across
  6 call sites; gains 1 alloc per error.

  ```rust
  // Sketch:
  pub(crate) fn prefix_message_2(err: &mut DbError, module: &str, context: &str) {
      match err {
          // ... prefix-eligible variants
          *message = format!("{module}: {context}: {message}");
      }
  }

  pub(crate) fn coded_sql_2(module: &str, context: &str, e: compio_postgres::Error) -> DbError {
      let mut err: DbError = e.into();
      prefix_message_2(&mut err, module, context);
      err
  }

  // Per-module wrappers shrink to:
  fn coded_sql(context: &str, e: compio_postgres::Error) -> DbError {
      crate::error::coded_sql_2("audit", context, e)
  }
  ```

  Verification: see r6 perf review §coded_sql alloc accounting.

  Severity: LOW — this is observational. The error path is cold;
  the gain is fractional. Mention here only because the prompt
  asked.

---

## 5. finalise_backfill tracing::warn quality

### [LOW] Operator context missing `name` + `collection` from the snapshot

`migrations.rs:644-657`:

```rust
if let Err(e) = backend
    .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
    .await
{
    tracing::warn!(
        app_id = %app_id,
        audit_id = audit_id,
        terminal = ?terminal,
        error = %e,
        "finalise_backfill failed; audit row may stay in 'running' \
         status until next reset() — investigate if the operator \
         sees stuck migrations"
    );
}
```

  Why: operator-debug impact — the message is good (names the
  consequence, names the workaround). The fields are good
  (`app_id` + `audit_id` is the join-key on `migration_audit`).
  But `lock_snapshot()` at `migrations.rs:459` destructures the
  snapshot tuple `(name, collection, audit_id, dry_run,
  start_generation)` — `name` and `collection` are in scope right
  here, free.

  An operator seeing this warn in a busy app has to:
  1. Read `audit_id` from the warn.
  2. Open the audit table.
  3. Look up the row to learn *which* migration stalled.

  Adding the two fields to the warn makes the line self-contained:

  ```rust
  tracing::warn!(
      app_id = %app_id,
      audit_id = audit_id,
      migration = %name,
      collection = %collection,
      terminal = ?terminal,
      error = %e,
      "finalise_backfill failed; audit row may stay in 'running' \
       status until next reset() — investigate if the operator \
       sees stuck migrations"
  );
  ```

  Fix: 2 extra structured fields. The strings already exist in the
  outer destructure at `migrations.rs:459`.

  Verification:
  ```
  grep -n 'lock_snapshot()' crates/plugin-db/src/migrations.rs
  # line 459 — destructures (name, collection, audit_id, ...)
  grep -n 'tracing::warn!' crates/plugin-db/src/migrations.rs
  # one hit at line 648 — the finalise_backfill warn
  ```

  Severity: LOW — the operator can still join via `audit_id`;
  this is a self-describability improvement, not a bug.

---

## 6. prefix_message wildcard arm + #[non_exhaustive] (security r6 INFO)

### [INFO] `error.rs:343` — `_ => {}` silently bypasses future variants

`error.rs:327-345`:

```rust
pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
    match err {
        DbError::UniqueViolation { message }
        | DbError::FkViolation { message }
        | DbError::NotNullViolation { message }
        | DbError::CheckViolation { message }
        | DbError::Serialization { message }
        | DbError::LockContention { message }
        | DbError::Transient { message }
        | DbError::Internal { message } => {
            *message = format!("{prefix}{message}");
        }
        // ValidationFailed / Configuration / Coded / SchemaRefused
        // carry their own structured messages and `.code`s the SDK
        // branches on; leaving them alone keeps the wire format
        // verbatim.
        _ => {}
    }
}
```

`DbError` is `#[non_exhaustive]` (`error.rs:55`). The wildcard arm
covers both today's four structured variants *and* any future
variant. If a new SQLSTATE classification is added to `from_pg`
(e.g. a `DataException { … }` variant for class `22` errors), the
prefix is silently dropped for its messages — operators lose the
`"audit: …: "` context phrase. The `.code` and message reach the
SDK intact (this is the same finding as security r6 §1.2 INFO).

  Why: SDK-author impact: none (`.code` is correct, message is
  correct, just unprefixed). Operator impact: low (the message
  body still carries the SQLSTATE text via `walk_pg_chain`, just
  without the module-scope prefix). Tripwire impact: when a future
  variant gets added, the reviewer doesn't get a forcing function
  to consider whether the variant should be prefixed.

  Fix: replace `_ => {}` with an explicit match arm for each
  structured variant. Forces a compile-time choice on every new
  variant:

  ```rust
  pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
      match err {
          DbError::UniqueViolation { message }
          | DbError::FkViolation { message }
          | DbError::NotNullViolation { message }
          | DbError::CheckViolation { message }
          | DbError::Serialization { message }
          | DbError::LockContention { message }
          | DbError::Transient { message }
          | DbError::Internal { message } => {
              *message = format!("{prefix}{message}");
          }
          // Explicit no-op: these variants carry contracted wire
          // bodies the SDK parses verbatim. A new DbError variant
          // forces the reviewer to extend one of these arms.
          DbError::ValidationFailed { .. }
          | DbError::Configuration { .. }
          | DbError::Coded { .. }
          | DbError::SchemaRefused { .. } => {}
      }
  }
  ```

  The same edit applies to `migrations.rs:82-102::coded_db` (a
  sixth copy of the variant-walker that takes typed `DbError` —
  same fragility, not consolidated by `cbbc9059`).

  Verification:
  ```
  grep -n '_ => {}' crates/plugin-db/src/error.rs crates/plugin-db/src/migrations.rs
  # 2 hits — error.rs:343, migrations.rs:99
  ```

  Severity: INFO — pure tripwire. No current bug; the security
  review classed it as a debt note.

---

## 7. lock_guard.release tracing::warn shape

### [PASS] Unlock-SQL warn carries key + tag + error

Re-verified at `orchestrator/lock_guard.rs:168-179` (no change from
r5):

```rust
if let Err(e) = client
    .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
    .await
{
    tracing::warn!(
        key = %self.key,
        tag = %self.tag,
        error = %e,
        "pg_advisory_unlock failed; session-scoped lock may stay \
         held until the pool recycles the connection"
    );
}
```

Sampled the operator-side fields produced:
- `key = "zs_reg:app_<base62-id>"` — names the affected app's
  advisory-lock namespace. Joins against the gateway's logs by
  app_id.
- `tag = "register_model"` — the pipeline stage. (Currently the
  only `tag` value in production; future stages would be visible
  here.)
- `error = "<DbError::Display body>"` — for a SQLSTATE-bearing
  backend failure this carries the `walk_pg_chain` output
  (`"db: <wrapper> — caused by: <cause>"`), so the operator sees
  what Postgres said. The SQLSTATE class is the leading text via
  the chain.

The companion Drop log at `lock_guard.rs:227-237` carries `key`
and `tag` (no `error` — we're in the panic-unwind / missed-release
path with no Result to inspect):

```
key = "zs_reg:app_42",
tag = "register_model",
"leak: OrchestratorLockGuard dropped without release()/into_held(); \
 session-scoped pg_advisory_lock will stay held until the pooled \
 client's PG session closes (typically on pool recycle). \
 Concurrent register_model callers for this app will stall in \
 the meantime. Either an async-cancellation hit the release().await, \
 a panic unwound the call stack, or a code path forgot to call \
 release()/into_held() — investigate."
```

  Verdict: operator-context complete. Both fields name *what*
  (the lock + stage) and the message names the *consequence*
  ("concurrent callers will stall") AND the diagnostic checklist
  ("async-cancel / panic / forgotten release"). This is the
  right shape for an operator-only event.

### [INFO] `.release().await` callers swallow Result via `let _ =`

Three call sites bare `let _ = lock_guard.release().await`:

- `register_model/apply.rs:226`
- `register_model/bootstrap.rs:137`
- `register_model/mod.rs:217`

The inner unlock-SQL warn fires from inside `release()` itself, so
the operator log gets the error. But the outer `let _ = ...`
makes it look like the release is infallible to readers — it is,
post-`ffb1e101` (the inner warn fires, then `Ok(client)` returns
unconditionally), but the call shape doesn't communicate that
contract. Cosmetic: a comment `// release() warns internally on
SQL-unlock error; outer Result is informational` next to each
`let _ =` would close the gap; the current docstring at
`lock_guard.rs:140-143` documents it once but not at the call
sites.

  Severity: INFO — readability only. No bug. Each site has its
  own justification comment already (`apply.rs:215-225` explains
  *why* the release runs there); this is a smaller addition.

---

## 8. SDK `withRetry` default predicate (cross-crate, out of scope)

### [MED-tracked] `sdks/db/src/with-retry.ts:25-30` — still matches only `optimistic_lock_failure`

Re-verified at HEAD:

```ts
export function isOptimisticLockError(e: unknown): boolean {
  return (
    e instanceof Error &&
    (e as { code?: unknown }).code === "optimistic_lock_failure"
  );
}
```

Native rail correctly stamps:
- `transient` + hint (`error.rs:235-239`)
- `serialization_failure` + hint (`error.rs:225-229`)
- `lock_not_available` + hint (`error.rs:230-234`)

None are picked up by the default `withRetry` predicate. SDK
callers get correctly classified errors with hints AND the default
retry loop silently bypasses them.

The example block in the `withRetry` docstring even *demonstrates*
the workaround:

```ts
await withRetry(() => db.x.update(...), {
  on: (e) => isOptimisticLockError(e) || (e as { code?: string }).code === "serialization_failure",
});
```

The default ought to be the OR'd predicate, not the bare
`optimistic_lock_failure` match. Out of scope for this review
(SDK crate), tracked here for visibility.

---

## 9. SDK `toJSON` drops `hint` (cross-crate, out of scope)

### [MED-tracked] `sdks/db/src/collection.ts:46-65` — `toJSON` does not surface `hint`

Re-verified at HEAD. The `toJSON` attached to Errors flowing
through `Result.error` enumerates `name`, `message`, `code`,
`errors` — NOT `hint`. Every hint the native rail mints
(retryable variants, `register_model` violations, the three
`session_*` codes if/when they get `validation_hinted`) is
visible in the live `Error` object but disappears at the RPC
`JSON.stringify` boundary.

This remains the single highest-impact 3-LOC fix in the project.

Out of scope (SDK crate), tracked here.

---

## 10. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC | Files | Status vs r5 |
|---|---|---|---|---|
| 1 | r5 §10#1 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts` | UNCHANGED |
| 2 | §2 — switch `auth/session` P0001 promotions to SQLSTATE-driven codes (`45001-45003` user-defined) + adopt `validation_hinted()` for all three | ~30 | `auth/session.rs`, the SECURITY DEFINER `init_session` SQL | NEW (folds in r5 §3) |
| 3 | r5 §10#2 — SDK `withRetry` default predicate add `transient` / `serialization_failure` / `lock_not_available` | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 4 | §3 — `WalConsumer::new` flatten — add a `CodedNotProvisioned { code, message }` variant to `ConsumerError`, preserve `.code` through dispatch | ~20 | `wal_consumer.rs`, `replication_ops.rs` | NEW |
| 5 | §5 — `finalise_backfill` warn: add `migration = %name` + `collection = %collection` fields | 2 | `migrations.rs:648-657` | NEW |
| 6 | r5 §10#5 — `parse_name_and_collection` / `parse_commit_spec` → typed `DbError::ValidationFailed { code: "invalid_migration_spec" }` | ~15 | `v8_classes/migration.rs`, `v8_classes/migrations.rs` | UNCHANGED |
| 7 | §6 — replace `_ => {}` in `prefix_message` (+ `migrations::coded_db`) with explicit structured-variant arms as tripwire for future variants | ~10 | `error.rs`, `migrations.rs` | NEW (folded from security r6 INFO) |
| 8 | r5 §10#9 — augment `migrations.rs:329` "migration insert returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs:326-332` | UNCHANGED |
| 9 | §4 — tighten `coded_sql` wrapper chain to a 2-arg helper (3 → 2 allocs on cold path) | ~15 | `error.rs` + 5 wrappers | NEW (cosmetic) |
| 10 | r5 §10#6 — stamp `retryable: true` wire flag on `OpError::coded` for the three retryable codes | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 11 | r5 §10#10 — either route `migrations::coded()` through `DbError::Coded` or delete the variant | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

---

## 11. Score

**87 / 100** (+2 vs r5's 85)

**What earned the +2:**

- **`cbbc9059` (5-site `coded_sql` dedup)** — closes r5 §2 LOW. The
  variant-walker now lives in one place; future drift between
  per-module helpers is structurally impossible. Real **+1**.
- **`51ced4a0` (finalise_backfill warn-on-err)** — closes an F1-family
  silent-error regression. The audit row stall is now observable
  to operators with `app_id`, `audit_id`, `terminal`, and `error`
  in the fields. The two missing fields (`name`, `collection`,
  §5) are a polish gap, not a bug. Real **+0.5**.
- **`f7d0961c` (preamble correction)** — pure documentation. Real
  **+0.5**.
- **`34d209b5` (ConsumerRunningGuard restructure)** — no error-UX
  delta. Real **+0**.

The window's commits are smaller than the [I28] sweep that drove the
r4→r5 jump; the score moves accordingly.

**Why not higher (the -13 deficit):**

- §2 / r5 §10#2 — `auth/session.rs` P0001 substring matching is the
  highest-value native-side fix remaining. SDK `.code` is correct
  today but coupled to a magic-string the compiler can't enforce.
  This is Pilot Pick #1 — when it lands with `validation_hinted()`,
  +3 trivially. Currently still in flight.
- §3 / r5 §10#4 — `WalConsumer::new` flattens `DbError` to
  `ConsumerError::NotProvisioned(String)`. Pre-existing fragility;
  Pilot Pick #2.
- §5 — finalise_backfill warn missing 2 in-scope fields.
- §6 — `prefix_message` wildcard arm tripwire.
- SDK-side: §8 and §9 are both blocking but cross-crate.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87). The +2 in this
window reflects the smaller commit set since r5; the two open
MAJORs (Pilot Picks #1 + #2) carry +3 and +2 respectively when
they land. The next cycle ceiling, assuming both Picks land plus
§5 and §6:

- Pilot Pick #1 (auth/session SQLSTATE + hints) → +3 (closes §2 + r5 §3)
- Pilot Pick #2 (ConsumerError code preservation) → +2 (closes §3)
- §5 (finalise_backfill warn fields) → +1
- §6 (prefix_message tripwire) → +1
- §9 cosmetic (coded_sql alloc tightening) → +0 (cosmetic only)

If all four land cleanly, next score should reach **94 / 100**.
The remaining 6 are SDK-side (`toJSON` hint, `withRetry`
predicate) — out of scope for plugin-db review and gated on the
SDK cycle.

The native rail is now closer to "done" than "in flight". The
remaining 13 points are split roughly 5 / 8 between this crate
and downstream — and the 5 are concentrated in three concrete
edits (Picks #1 + #2 plus the §5/§6 polish).
