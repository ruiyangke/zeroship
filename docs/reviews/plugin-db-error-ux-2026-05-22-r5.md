# plugin-db Error-UX Review — 2026-05-22 r5

Scope: `crates/plugin-db/src/` at HEAD (post `0049d9be`, `91830cca`,
`808a32af`, `ffb1e101`, `c0590506`, `eda96ead`, `e399eeea`,
`a0fec06a`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r4.md` (77 / 100).

Lens: SDK-author error-handling discipline. Every finding evaluates the
JS-visible surface — `e.code`, `e.message`, `e.hint` — and whether the
SDK can branch on it without parsing strings.

---

## TL;DR (changes since r4)

**Resolved since r4 — verified at HEAD:**

- **r4 §1.1 HIGH (`replication.rs` returns `Result<_, String>`; 4 dispatch
  sites flatten to `.code = "internal"`)** — CLOSED by the
  [I28] sweep (`0049d9be`, `91830cca`) and `a0fec06a`. The four
  `replication::*` helpers now return `Result<_, DbError>`; dispatch
  sites at `replication_ops.rs:79-83,118-122,164-168,222-227` route
  via `e.to_op_error()` — SDK now sees `wal_level_not_logical` /
  `invalid_app_id` / SQLSTATE-derived codes (`transient`,
  `lock_not_available`, `unique_violation`) verbatim. The largest
  remaining `.code` discipline gap of r4 is closed end-to-end.
  Verification:
  ```
  grep -n 'DbError::Internal { message: e }' crates/plugin-db/src/replication_ops.rs  # zero
  grep -n 'Result<.*, String>' crates/plugin-db/src/replication.rs       # zero in fn sigs
  ```
- **r4 §6 partial — `auth/*` typed sweep** — `0049d9be` converted
  `auth/bootstrap.rs` (`ensure_admin_schema` + helpers), `auth/keys.rs`
  (`rotate_session_keys`, `current_key_id`, `previous_key_id`), and
  `auth/session.rs` (`mint_session_token`, `init_session`,
  `mint_and_init*`) to `Result<_, DbError>`. All three modules carry
  per-module `coded_sql` helpers prefixed `"auth/{module}: <ctx>: "`.
- **3 P0001 RAISE promotion** — `auth/session.rs:215-229` now promotes
  the three SECURITY DEFINER refusals (`nonce replay detected` →
  `session_nonce_replay`, `signature expired` →
  `session_signature_expired`, `invalid session-init signature` →
  `session_invalid_signature`) to typed `DbError::validation(...)` with
  stable static codes. Integration tests
  (`tests/integration.rs:3608-3697`) pin the contract on a live pg-test
  cluster.
- **r4 §1.2 (`coded_db` doubled `"db: "` prefix)** — re-verified clean
  at `migrations.rs:82-102`. Inline source-history comment preserved.
- **r4 §6 cosmetic (`validate.rs` SchemaRefused doc)** — re-verified
  accurate at `validate.rs:16-32`. Doc explicitly describes the
  `Result<_, String>` envelope being wrapped in
  `DbError::SchemaRefused` at `register_model::mod.rs:200-208`.
- **r4 §9 (OpError envelope shape stability)** — re-verified clean.
  Every JS-visible reject lands as `OpErrorKind::CodedError { code,
  hint }` materialised at `runtime/src/core/runtime.rs:2101-2114` as a
  plain `Error` with `.code` (string) and optional `.hint` (string).
  Shape unchanged across the r4→r5 window.
- **`tx_connect_failed`** — re-verified routed via `e.to_op_error()`
  at `migrations.rs:259-267`. Zero hits in source.
- **auto_tx `OpResult::JsValue` + `RejectError`** — re-verified at
  `orchestrator/auto_tx.rs:120-138` (`begin_to_resolve_value` /
  `end_to_resolve_value`). COMMIT-time DbError code/hint preserved.
- **Empty-RETURNING helper** — `eda96ead` extracted
  `crate::error::first_row_or_internal` and audit.rs now uses it at
  three sites (`audit.rs:314,600`, plus migration audit row). The
  message format `"<op>: returned no row"` is consistent.
- **Lock-guard unlock-SQL warn / Drop log** — `ffb1e101` + `808a32af`
  delivered both: `lock_guard.rs:152-158` `tracing::warn!(key, tag,
  error)` on unlock-SQL failure; `lock_guard.rs:207-217`
  `tracing::error!(key, tag)` on missed-release Drop. Operator
  observability for lock leaks is now complete.

**Still open from r4 (unchanged):**

- r4 §7 HIGH — SDK `toJSON` drops `hint`. Out-of-scope (SDK fix), but
  the native rail now has more hints to surface.
- r4 §5 MEDIUM — SDK `withRetry` default predicate matches **only**
  `optimistic_lock_failure`. Native rail correctly stamps
  `transient` / `serialization_failure` / `lock_not_available` with
  retry hints; SDK doesn't pick them up.
- r4 §4 — `validation_hinted` still has **zero** production callers.
  The 3 P0001 promotions (§3 below) added in this round are textbook
  candidates — they use `DbError::validation()` (no hint) where a
  hint would help.
- r4 §8 — `DbError::Coded` variant still production-unused.
- r4 §3 — `migrations.rs:329` `"db: migration insert returned no row"`
  still lacks the `(app_id, collection, name)` triple; sibling
  audit.rs sites adopted `first_row_or_internal` but this one didn't.

**Net score delta vs r4:** see §10.

---

## 1. Verify r4 closures

### [PASS] tx_connect_failed routes via to_op_error

`migrations.rs:259-267`:

```rust
let client = backend
    .acquire_dedicated_client()
    .await
    // Route through `to_op_error()` so the SQLSTATE-derived code
    // (`transient`, etc.) and its retry hint reach the SDK, rather
    // than collapsing every connection failure to `tx_connect_failed`.
    // Restored from 60ca1ad6 — silently reverted by ed697c45 during
    // the audit-rail refactor; caught by error-ux r3.
    .map_err(|e| e.to_op_error())?;
```

Verification: `grep -rn 'tx_connect_failed' crates/plugin-db/src/` →
zero hits in production source (only in the inline comment above).

### [PASS] coded_db no longer doubles "db: "

`migrations.rs:82-102` matches the inline comment — single `"{context}:
{message}"` prefix. The variant-walking match excludes the structured
codes (Configuration, ValidationFailed, Coded, SchemaRefused) so their
.code-meaningful messages aren't perturbed.

### [PASS] validate.rs SchemaRefused docs accurate

`validate.rs:16-32`:
- States `Result<_, String>` is the lone exception (the envelope is
  the SDK wire contract).
- Cites `register_model::run_pipeline` as the wrapping site.
- States `to_op_error()` stamps `.code` from the static
  discriminator AND emits envelope as `Error.message`.

Cross-check at `register_model/mod.rs:194-208`: the `Err(envelope_json)`
is wrapped in `DbError::SchemaRefused { code: "validation_refused",
envelope_json }` as documented.

### [PASS] auto_tx OpResult::JsValue + RejectError

`auto_tx.rs:60-72` (begin) and `auto_tx.rs:97-108` (end): both spawn
into `OpResult::JsValue { value, … }`. The conversion helpers
`begin_to_resolve_value` and `end_to_resolve_value` (lines 120-138)
extract via `to_op_error()` so COMMIT/ROLLBACK errors preserve
`.code` + `.hint`.

The unit tests at `auto_tx.rs:298-360` pin the conversion contract
without a V8 scope.

---

## 2. [I28] sweep verification

Sampled 8 paths end-to-end. All convert correctly to `Result<_, DbError>`.

| # | Path | Status | Code reaches SDK? |
|---|---|---|---|
| 1 | `auth::ensure_admin_schema` (`bootstrap.rs:77`) | typed | yes (via callers' `to_op_error()`) |
| 2 | `auth::rotate_session_keys` (`keys.rs:77`) | typed | yes |
| 3 | `auth::current_key_id` / `previous_key_id` (`keys.rs:113,148`) | typed | yes |
| 4 | `auth::mint_session_token` (`session.rs:95`) | typed | yes |
| 5 | `auth::init_session` (`session.rs:178`) | typed | yes (3 stable codes) |
| 6 | `replication::ensure_publication_and_slot` (`replication.rs:192`) | typed | yes (`wal_level_not_logical` etc.) |
| 7 | `replication::watchdog_query` (`replication.rs:400`) | typed | yes |
| 8 | `replication::drop_abandoned_slots` (`replication.rs:511`) | typed | yes |

### [LOW] `crates/plugin-db/src/replication_ops.rs` — no JS-boundary callers wire `auth/*` codes yet

  Why: SDK-author impact — the 3 P0001 RAISE codes
  (`session_signature_expired` / `session_nonce_replay` /
  `session_invalid_signature`) and the auth-bootstrap codes are
  end-to-end typed in Rust but have no V8 dispatch surface (no
  v8_class wraps `auth::init_session` / `ensure_admin_schema`).
  They're called from control-plane bootstrap and integration tests
  only. The `.code` discipline is correct, but no SDK author can
  currently see it. This is not a regression — it's a
  reachability observation.

  Fix (optional): when the auth surface is exposed to JS (likely
  `zeroship.auth.*` namespace or a dedicated control-plane RPC),
  the typed errors will flow verbatim through the same
  `to_op_error()` boundary the rest of the crate uses.

  Verification:
  ```
  grep -rn 'init_session\|mint_and_init\|ensure_admin_schema' \
    crates/ --include='*.rs' | grep -v '/.claude/' | grep -v '/plugin-db/'
  # zero hits
  ```

### [LOW] Four duplicated `coded_sql` helpers

`auth/bootstrap.rs:21-37`, `auth/keys.rs:34-56`, `auth/session.rs:27-49`,
and `diff.rs:37-53` each re-implement the same variant-walking
prefix wrapper. `crate::error::coded_sql` (`error.rs:353-357`) is the
shared helper that does this work via the central
`prefix_message`; `audit.rs:58-60` delegates correctly. The four
duplicates predate the central helper and could be deleted (8-10 LOC
each × 4 sites = ~40 LOC removed). Not a correctness gap — they all
produce identical output — but a maintenance hazard if `prefix_message`'s
match arms ever drift.

Verification:
```
grep -n '^fn coded_sql' crates/plugin-db/src/auth/*.rs \
  crates/plugin-db/src/diff.rs crates/plugin-db/src/audit.rs
# four hits in {auth/bootstrap,auth/keys,auth/session,diff}.rs
# audit.rs delegates via a one-liner wrapper
```

---

## 3. 3 P0001 RAISE promotion — verification

### [PASS] Codes promoted; integration tests pin contract

`auth/session.rs:213-235`:

```rust
if msg.contains("nonce replay detected") {
    DbError::validation("session_nonce_replay",
        "auth/session: nonce replay detected")
} else if msg.contains("signature expired") {
    DbError::validation("session_signature_expired",
        "auth/session: signature expired")
} else if msg.contains("invalid session-init signature") {
    DbError::validation("session_invalid_signature",
        "auth/session: invalid signature")
} else {
    coded_sql("init_session", e)
}
```

`tests/integration.rs:3608-3697` exercises each via a real
SECURITY DEFINER call and asserts the exact `.code` strings.

### [LOW] None of the three carry a `hint`

The three RAISE-promotions use `DbError::validation(code, message)`
— no hint. Recovery advice would help SDK authors:

- `session_signature_expired` — "re-mint via `mint_session_token`
  before init; tokens have a 5-min default TTL".
- `session_nonce_replay` — "the nonce has been used; mint a fresh
  token and retry".
- `session_invalid_signature` — "the token was minted against a
  retired key or tampered with; refresh credentials".

Why: SDK-author impact — without a hint, the wrapper SDK either
hand-rolls advice or surfaces an opaque "auth/session: …" body.
`DbError::validation_hinted` (`error.rs:287-297`) is the
canonical helper; this would be its first production caller
(closes r4 §4 partial gap).

Verification: `grep -n 'validation_hinted' crates/plugin-db/src/`
→ one declaration in `error.rs`, zero call sites.

---

## 4. lock_guard tracing::warn shape

### [PASS] Unlock-SQL warn carries key + tag + error

`orchestrator/lock_guard.rs:148-159`:

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

Operator context: full. The `key` field (`"zs_reg:<app_id>"`)
identifies the affected app; `tag` (`"register_model"`) names the
stage; `error` is the typed DbError Display — for a SQLSTATE
backend failure this carries the SQLSTATE-derived classifier
prefix.

### [PASS] Drop fallback log carries key + tag

`orchestrator/lock_guard.rs:207-217`: `tracing::error!(key, tag,
"leak: …")` with the full operator-facing message including the
remediation context ("Postgres releases when backend session
terminates"; "concurrent register_model callers will stall";
"investigate async-cancel / panic / forgotten release").

### [LOW] `tracing::warn` does not surface to JS

The unlock-SQL failure path is unilateral best-effort (per the
documented contract in `lock_guard.rs:120-123`), so JS never sees
the warn. This is correct — release() returns the unlocked client
without propagating the unlock error. But it means an SDK author
debugging a session-lock-held-forever bug has to read worker logs,
not the JS exception. Not a bug; an observability note.

---

## 5. Hint discipline — newly-added DbError variants

Sweep of new `DbError::*` variants introduced in [I28]:

| Site | Variant | Hint? | Severity |
|---|---|---|---|
| `auth/session.rs:215-219` | `ValidationFailed { code: "session_nonce_replay" }` | NO | MED — recovery is "mint a fresh token" |
| `auth/session.rs:220-224` | `ValidationFailed { code: "session_signature_expired" }` | NO | MED — recovery is "re-mint, TTL exceeded" |
| `auth/session.rs:225-229` | `ValidationFailed { code: "session_invalid_signature" }` | NO | MED — recovery is "key rotated; refresh creds" |
| `replication.rs:107-122` | `ValidationFailed { code: "invalid_app_id" }` | NO | LOW — operator-shaped error, less actionable |
| `replication.rs:282-289` | `Configuration { code: "wal_level_not_logical" }` | NO | MED — message body has fix, but not in `hint` slot |
| `auth/keys.rs` `DbError::internal(...)` (multiple) | `Internal` | NO (none possible) | n/a |
| `auth/session.rs` `DbError::internal(...)` (multiple) | `Internal` | NO (none possible) | n/a |

The 3 RAISE promotions are the highest-impact hint opportunities — an
SDK author seeing `e.code === "session_signature_expired"` doesn't
know if it's "TTL exceeded, re-mint" or "key rotated, refresh creds"
without the message body (which mixes operator-facing context). A
hint would disambiguate.

`wal_level_not_logical` already carries the fix INSIDE the message
body ("set wal_level=logical in postgresql.conf and restart") — that
text belongs in `hint`, not message. Currently it reads:

```
"replication: server is not configured for logical decoding — set
 wal_level=logical in postgresql.conf and restart (underlying: …)"
```

Better:
```
message: "replication: server is not configured for logical decoding"
hint:    "set wal_level=logical in postgresql.conf and restart"
```

This is the same `validation_hinted` → `Configuration { hint }` shape
gap that has been re-flagged since r3.

---

## 6. SDK retry compatibility

### [MED] Native rail correct; SDK predicate stale (unchanged from r4 §5)

Re-verified at HEAD. `sdks/db/src/with-retry.ts:25-30`:

```ts
export function isOptimisticLockError(e: unknown): boolean {
  return (
    e instanceof Error &&
    (e as { code?: unknown }).code === "optimistic_lock_failure"
  );
}
```

Native rail stamps:
- `transient` + hint "transient backend failure; retry after a short
  backoff" (`error.rs:231-235`)
- `serialization_failure` + hint "retry the transaction; Postgres SSI
  / deadlock detector aborted it" (`error.rs:221-225`)
- `lock_not_available` + hint "Retry after a short backoff; another
  worker holds the lock briefly" (`error.rs:226-230`)

None are picked up by the default `withRetry` predicate. SDK callers
get correctly classified errors with hints AND the default retry
loop silently bypasses them.

Why MED (carried over): scope is SDK, not plugin-db. Native side
discharged its responsibility — the issue is now wholly downstream.

---

## 7. OpError envelope shape stability

### [PASS] No regressions in r4→r5 window

`OpErrorKind::CodedError { code: String, hint: Option<String> }`
unchanged at `runtime/src/core/state.rs:70`. JS materialisation at
`runtime/src/core/runtime.rs:2101-2114` produces:

```js
{
  // Plain Error (name: "Error")
  message: <DbError::Display body>,
  code: <variant-canonical or static>,
  hint: <Option<String>; omitted when None>
}
```

Verified across every `ResolveValue::RejectError(...)` site
(16 across `crud.rs`, `replication_ops.rs`, `auto_tx.rs`,
`register_model/mod.rs`, `orchestrator/transaction.rs`,
`v8_classes/{migrations,migration,subscription}.rs`).

The two exceptions are deliberate `TypeError` rejects from
`v8_classes/migration.rs::parse_commit_spec` and
`v8_classes/migrations.rs::parse_name_and_collection` (§8 below).

---

## 8. Remaining gaps — V8-boundary `TypeError` shape lacks `.code`

### [LOW] `parse_name_and_collection` / `parse_commit_spec` reject as `TypeError` with no `.code`

`v8_classes/migrations.rs:148-156`:

```rust
let (name, collection) = match parse_name_and_collection(scope, spec) {
    Ok(pair) => pair,
    Err(msg) => {
        let m = v8::String::new(scope, &msg).unwrap();
        let exc = v8::Exception::type_error(scope, m);
        resolver.reject(scope, exc);
        return promise;
    }
};
```

Similar at `v8_classes/migration.rs:369-377` (`parse_commit_spec`).
Both reject with a plain `TypeError` whose `.message` is the only
SDK signal — no `.code`. SDK must substring-match
`"migrations: spec.name must be a non-empty string"` /
`"db: commitBatch: spec.updates must be an array"`.

The rest of the crate's user-input validators route through
`DbError::ValidationFailed` (or `DbError::from(QueryError)`)
which gives `.code = "invalid_filter"` / `"invalid_collection"`
etc. The V8-boundary parsers diverge from that convention.

Why: SDK-author impact — these are user-input-shape refusals,
matched semantics with `invalid_filter` etc. They should stamp
codes like `"invalid_migration_spec"` (or per-field
`"missing_name"` / `"missing_collection"` / `"updates_not_array"`
etc.).

Fix: convert both helpers to return `DbError`, materialise via
`to_op_error()` at the dispatch boundary. ~15 LOC across two
files.

Verification:
```
grep -n 'v8::Exception::type_error.*&msg' \
  crates/plugin-db/src/v8_classes/migration.rs \
  crates/plugin-db/src/v8_classes/migrations.rs
# two hits — both at parse_* error paths
```

### [LOW] `DbError::from(QueryError)` strips hint info from invalid_filter

`error.rs:425-440`:

```rust
DbError::ValidationFailed {
    code,
    message: msg,
    hint: None,
}
```

QueryError variants (`InvalidFilter`, `InvalidCollection`,
`InvalidIdent`) each have well-known recovery patterns
(e.g. "filter nesting cap exceeded; flatten $and/$or", "collection
name must match `[A-Za-z_][A-Za-z0-9_]*`"). Currently the messages
mix recovery text with the body. Same `validation_hinted` gap as
§3 above.

---

## 9. Inconsistency notes

### [LOW] `coded_db` (migrations) vs `coded_sql` (auth/diff/audit) — different shapes

- `migrations.rs::coded_db(context, DbError) -> OpError`: takes typed
  DbError, prepends `"{context}: "` then converts to OpError.
- `crate::error::coded_sql(context, compio_postgres::Error) -> DbError`:
  takes raw pg::Error, converts then prepends.
- `auth/{bootstrap,keys,session}.rs::coded_sql` and
  `diff.rs::coded_sql`: same signature as the shared `coded_sql`,
  but re-implement the prefix walk locally.

Five `coded_sql`-shaped helpers + one `coded_db`. Convergence
opportunity:
```rust
crate::error::coded_sql(context, e)       // single helper
crate::error::wrap_sql(context, err)      // takes typed DbError
```

This is a code-organisation note, not an SDK-visible bug. The output
shapes are uniform.

---

## 10. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC | Files | Status vs r4 |
|---|---|---|---|---|
| 1 | r4 §7 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts` | UNCHANGED (r4 rank 1) |
| 2 | r4 §5 — SDK `withRetry` matches only `optimistic_lock_failure` | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED (r4 rank 3) |
| 3 | §3 — add hints to 3 P0001 promotions (`session_*`) + `wal_level_not_logical` + `invalid_app_id` | ~25 | `auth/session.rs`, `replication.rs` | NEW — first-time-use site for `validation_hinted` (closes r4 #9 partial) |
| 4 | r4 §4 — add hints to `migration_cancelled` (mid-run), `tx_settled`, `invalid_isolation_level`, `not_configured`, `From<QueryError>::invalid_filter` | ~25 | `migrations.rs`, `transaction.rs`, `error.rs` | UNCHANGED |
| 5 | §8 — convert `parse_name_and_collection` + `parse_commit_spec` to `DbError`-stamped codes (`invalid_migration_spec`) | ~15 | `v8_classes/migrations.rs`, `v8_classes/migration.rs` | NEW — surfaced post-r4 sweep |
| 6 | r4 §5 — stamp `retryable: true` on `OpError::coded` for `transient`/`serialization_failure`/`lock_not_available` | ~15 | `error.rs` + `state.rs` | UNCHANGED |
| 7 | §2 — consolidate 4 duplicate `coded_sql` helpers onto central `crate::error::coded_sql` | ~40 LOC removed | `auth/{bootstrap,keys,session}.rs`, `diff.rs` | NEW — code-organisation hygiene |
| 8 | r4 §6 — `cic_failed` envelope code collision | ~10 | `backend/postgres.rs` | UNCHANGED |
| 9 | r4 §3 — augment `migrations.rs:329` `"migration insert returned no row"` with `(app_id, collection, name)` triple; consider plumbing through `first_row_or_internal` (the empty-RETURNING helper from `eda96ead`) | ~5 | `migrations.rs` | UNCHANGED (audit.rs sites now use the helper; this one didn't migrate) |
| 10 | r4 §8 — either route `migrations::coded()` through `DbError::Coded` or delete the variant | ~30 sites OR ~5 LOC | `migrations.rs`, `error.rs` | UNCHANGED |

---

## 11. Score

**85 / 100** (+8 vs r4's 77)

**What earned the +8:**

- r4 §1.1 HIGH (`replication.rs` → typed errors) — the biggest
  remaining `.code` discipline gap at r4 — closed end-to-end via
  `0049d9be` + `91830cca` + `a0fec06a`. The change is principled
  (`Result<_, DbError>` flows verbatim through `to_op_error()`),
  variant-rich (`wal_level_not_logical` → `Configuration`,
  `invalid_app_id` → `ValidationFailed`, SQLSTATE-derived via
  `from_pg`), and the dispatch boundary needs zero variant
  re-mapping. Real **+4**.
- r4 §6 partial (auth/* typed sweep) — `0049d9be` converted the
  bootstrap + keys + session helpers to `Result<_, DbError>`. Even
  though no V8-boundary surface exposes them yet (§2 LOW), the
  invariant is in place for when control-plane RPC or
  `zeroship.auth.*` lands. Real **+1**.
- 3 P0001 RAISE promotions — `auth/session.rs` now stamps three
  stable codes (`session_signature_expired` / `session_nonce_replay`
  / `session_invalid_signature`) with integration-test pinning.
  This is the *right* shape (typed `ValidationFailed`, not opaque
  SQLSTATE 50000) and it's the only place in the crate where a
  Postgres-side `RAISE` message is promoted to a JS-visible
  `.code`. Real **+1**.
- `eda96ead` (`first_row_or_internal` helper) standardises the
  empty-RETURNING surface across audit.rs three call sites.
  Closes a tail of r3 §3 (the inconsistent empty-RETURNING
  fallback prose). Real **+1**.
- Lock-guard tracing discipline (`ffb1e101` warn-on-unlock-error +
  `808a32af` `#[must_use]` + Drop log) — pure observability;
  doesn't move the SDK-facing `.code` rail but raises the
  operator-side error UX floor. Real **+1**.

**Why not higher (the -15 deficit):**

- `sdks/db/src/collection.ts` `toJSON` STILL drops `hint`. Every
  hint added natively (and there were several this round) is
  invisible past the RPC `JSON.stringify` boundary until this
  lands. Highest-impact 3-LOC fix in the project.
- 3 P0001 promotions added with `DbError::validation()`, not
  `validation_hinted()`. Same gap r4 flagged — but this round the
  miss is "added new sites without using the unused helper" rather
  than "old sites never got hints". Net neutral; the helper-with-zero-callers
  count is the same.
- `wal_level_not_logical` embeds recovery advice in the message
  body instead of the `hint` slot — symptomatic of the same gap.
- `parse_name_and_collection` / `parse_commit_spec` reject with
  plain `TypeError` + substring-shaped message instead of the
  crate-wide `DbError::ValidationFailed` convention. A small
  divergence at the v8_class entry-point parsers.
- SDK `withRetry` default predicate still only matches
  `optimistic_lock_failure`.

**What the trajectory looks like:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) is +13, +6, +8. The r5 jump
is unusually large because the biggest r4 HIGH (`replication.rs`
typing) was bundled with the `auth/*` typing into one [I28] sweep
commit. The remaining gaps are increasingly fine-grained — the
"big lever" items are now all on the SDK side, not the native side.

**Projected next-round ceiling:**

- §10 #1 (SDK `toJSON` hint, 3 LOC) → +4
- §10 #2 (SDK `withRetry` default predicate, ~15 LOC) → +3
- §10 #3 (hints on 3 P0001 + wal_level + invalid_app_id) → +2
- §10 #5 (V8-entry parsers → typed DbError) → +2
- §10 #6 (`retryable: true` wire flag) → +1
- §10 #7 (consolidate 4 duplicate `coded_sql` helpers) → +1

If all of the above land cleanly, next score should reach
**98 / 100**. The remaining 2 are reserved for:
- the public error-codes catalog doc
  (`docs/reference/db-error-codes.md`), and
- a typed `ValidationRefusedError` SDK subclass.

Neither is medium-effort enough to ship inside a normal cycle.
