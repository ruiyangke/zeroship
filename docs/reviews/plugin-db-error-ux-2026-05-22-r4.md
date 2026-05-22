# plugin-db Error-UX Review — 2026-05-22 r4

Scope: `crates/plugin-db/src/` at HEAD (post `dec2bd42`, `07205e54`,
`5ceb6daa`, `37e61803`, `4cbe9fa1`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r3.md` (71 / 100).

Lens: SDK-author error-handling discipline. Every finding evaluates the
JS-visible surface — `e.code`, `e.message`, `e.hint`, `e.cause` — and
whether the SDK can branch on it without parsing strings.

---

## TL;DR (changes since r3)

**Resolved since r3 — verified at HEAD:**

- **r3 §1.1 HIGH (`tx_connect_failed` SQLSTATE strip)** — closed by
  `dec2bd42`. `migrations.rs:259-267` now reads
  `.map_err(|e| e.to_op_error())?`, with an inline comment explicitly
  citing the ed697c45 silent-revert and the r3 catch. Verification:
  `grep -n 'tx_connect_failed' crates/plugin-db/src/` → zero hits.
- **r3 §7 HIGH (`coded_db` doubled `"db: "` prefix)** — closed by
  `dec2bd42`. `migrations.rs:82-102` now prepends only
  `"{context}: {message}"`, with the same inline citation. Operator logs
  now read `"migration insert: db: <pg-msg>"` (single prefix), not
  `"db: migration insert failed: db: <pg-msg>"`.
- **r3 §6 cosmetic — `validate.rs` preamble lie about `SchemaRefused.code`**
  — closed by `07205e54`. `validate.rs:18-33` now accurately states the
  envelope is wrapped in `DbError::SchemaRefused` at the boundary, that
  `to_op_error()` stamps `.code = "validation_refused"`, and that the
  message body remains the envelope JSON for SDK `JSON.parse`. The doc
  matches the code in `error.rs:194-205` and is no longer misleading.

**Still open from r3 (unchanged):**

- **r3 §1.2 MEDIUM** — `replication.rs` returns `Result<_, String>`; the
  4 dispatch sites in `replication_ops.rs:84,118,155,215` wrap as
  `DbError::Internal`, collapsing wal_level / publication-exists / pool
  failures all to `.code = "internal"`. See §1.1 below.
- **r3 §3** — `DbError::validation_hinted` still has **zero**
  production callers. See §4.
- **r3 §6** — `cic_failed` JS-visible code still collides with the
  envelope-internal `"code": "validation_refused"` / `"unique_violation"`.
  See §6.
- **r3 §7 HIGH (SDK `toJSON` drops `hint`)** — still open at
  `sdks/db/src/collection.ts:46-66`. See §7.
- **r3 §8** — `DbError::Coded` variant declared but production-unused.
- **r3 §3** — three `"X returned no row"` Internal messages still lack
  the `(app_id, collection, name)` triple.

**Net score delta vs r3:** see §10.

---

## 1. `.code` discipline

### [HIGH] `crates/plugin-db/src/replication_ops.rs:84,118,155,215` — 4 dispatch sites still flatten `replication::*` to `.code = "internal"`

Unchanged since r3, re-listed at HIGH because the gap is now the
**largest remaining `.code` discipline gap in the crate** — every other
`.code` regression r2/r3 surfaced has been fixed.

```rust
match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
    Ok(out) => OpResult::JsValue { … ResolveValue::String(out.to_json()) … },
    Err(e) => OpResult::JsValue {
        resolver,
        value: ResolveValue::RejectError(
            DbError::Internal { message: e }.to_op_error()
        ),
        request_id,
    },
}
```

`crate::replication::ensure_publication_and_slot` returns
`Result<SetupOutcome, String>`. Inside the module the wal_level path is
carefully classified into a recognisable prose prefix
(`replication.rs:224-232`):

```rust
if msg.contains("55000") || msg.to_lowercase().contains("wal_level") {
    format!(
        "replication: server is not configured for logical \
         decoding — set wal_level=logical in postgresql.conf …"
    )
}
```

…but it's a `String` by the time it reaches the dispatch, where it
gets re-stuffed into `DbError::Internal`. JS sees `.code = "internal"`
for a wal_level mis-config (operator action). Same for `42710`
(publication exists), pool acquisition failure, every other branch.

**Why HIGH now (was MEDIUM in r3):** with r3's regressions reverted,
this is the **only remaining site in `plugin-db` where SDK callers
cannot distinguish "transient backend failure" (retry) from
"operator-actionable misconfiguration" (don't retry)**. Every other
exec rail now stamps a usable `.code`. The asymmetry is starting to
matter: a tenant's `db.replication.setup()` failing for `wal_level`
gets retried by `withRetry({ on: e => e.code === "transient" })`
silently — but every retry takes the same fatal `55000`/`08006` path,
amplifying the operator-actionable signal into log noise.

**Fix**: convert `replication.rs` to `Result<_, DbError>`. The current
`Result<_, String>` rail already classifies the prose prefix at three
sites (`wal_level` mis-config → `Configuration { code:
"replication_wal_level_misconfigured" }`, `42710` → `Coded { code:
"publication_exists" }`, `pg_create_logical_replication_slot returned
no row` → `Internal { … }`). The conversion is mechanical at four
return sites in `replication.rs` and removes the dispatch-side
`DbError::Internal` wrappers. ~30 LOC.

**Verification**:
```
grep -n 'Result<.*, String>' crates/plugin-db/src/replication.rs
# 7 hits — every public function and most helpers
grep -n 'DbError::Internal { message: e }' crates/plugin-db/src/replication_ops.rs
# 4 hits — every dispatch wraps the unstructured String
```

---

### [LOW] `crates/plugin-db/src/migrations.rs:320-331` — manual `coded("internal", …)` (carried over from r2 §1c, r3 §1.3)

Unchanged since r3. The empty-RETURNING guard for `insert_backfill_running`
uses `coded("internal", "db: migration insert returned no row", None)`
rather than `coded_db` or `DbError::Internal`. The wire shape is
unchanged (`.code = "internal"`); message body lacks `(app_id,
collection, name)` triple. Cosmetic re-list.

---

## 2. Remaining `OpResult::Failed { error: String }` sites

**Zero.** Confirmed fresh by:

```
grep -n 'OpResult::Failed' crates/plugin-db/src/
crates/plugin-db/src/error.rs:247:    /// `Result<_, String>` -> `OpResult::Failed { error: String }` path
crates/plugin-db/src/orchestrator/auto_tx.rs:64:        // legacy `OpResult::Failed { error: String }` rail flattened the
crates/plugin-db/src/orchestrator/auto_tx.rs:298:    //! `OpResult::Failed { error: e.into_string() }`, stripping `.code` /
```

All three hits are doc comments. No production caller remains. As in
r3, the downstream `OpResult::Failed` enum variant can be retired from
`crates/runtime/src/core/state.rs` on plugin-db's behalf (cross-crate;
out of scope).

Also confirmed fresh: no `coded(..., &e.into_string(), ...)` patterns
remain. `DbError::into_string` is used only at:

- `exec.rs:342` — test-only `exec_mutation_with_emit_for_tests`
  (`cfg(any(test, feature = "test-helpers"))`).
- `apply.rs:177` — render typed error to flat string for the
  `audit_error` column. Legitimate: the audit column is a text
  field, not a JS-visible surface.
- `replication.rs:248,652` — empty-RETURNING site flowing through
  `?` into the `Result<_, String>` rail. The conversion eats the
  typed variant; closure tracked in §1.1.
- `v8_bridge.rs:325` — `fmt_db_err` legacy helper.

The audit `apply.rs:177` and `v8_bridge.rs:325` sites are correct: the
former feeds a SQL column, the latter is the canonical
"render Postgres error for log" helper.

---

## 3. Error message quality — re-sample of 8 sites at HEAD

| # | Message | Source | Rating | Notes |
|---|---|---|---|---|
| 1 | `"replication: pg_create_logical_replication_slot returned no row"` | `replication.rs:243-249` | OK | Stable since `c83d6a8c`. Names the operation. Could add `(app_id, slot_name)`. |
| 2 | `"audit: insert_backfill_running returned no row"` | `audit.rs:617` | POOR | Same as r3. Operator must grep the audit log to find the failing `(app, collection, name)`. |
| 3 | `"audit: INSERT returned no row"` | `audit.rs:329` | POOR | Same as r3. The generic of the three. |
| 4 | `"db: migration insert returned no row"` | `migrations.rs:329` | POOR | Same as r3. Surrounding scope has `(app_id, name, collection)`; the message doesn't. |
| 5 | `"db: apply received a {kind} op outside ChangeClass::Destructive (class={:?}); upstream destructive-class filter (register_model::apply pass1/pass2) should have skipped it. …"` | `apply.rs:278-285` | GOOD | Carries the breached invariant + the file + the operator impact. Stable. |
| 6 | `"db: create index '{name}' exhausted retry budget without a terminal result"` | `backend/postgres.rs:597-600` | GOOD | Names the index, classified as `cic_configuration`. |
| 7 | `"this Migration wrapper has finalised"` (`migration_not_active`) | `migrations.rs:182-194` | GOOD | Carries recovery hint inline; model for the others. |
| 8 | `"db: apply received a DropColumn op outside ChangeClass::Destructive"` (mirror of #5) | `apply.rs:277-293` | GOOD | Same as #5; the destructive-invariant guard family is well-shaped. |

### Recurring offenders (unchanged from r3)

1. **"X returned no row"** at 3 sites (`audit.rs:329`, `audit.rs:617`,
   `migrations.rs:329`) — none carry the `(app_id, collection,
   name)` triple.

2. **Inner-error leakage** (`auto_tx.rs:203` `format!("db: auto-tx
   connect failed: {e}")`, `transaction.rs:150` `format!("db: tx connect
   failed: {e}")`, `register_model/mod.rs:121` `format!("db: lazy init
   failed: {e}")`) — `compio_postgres::Error`'s Display interpolated
   unconditionally. Same as r3.

3. **Mixed `"db: "` prefix shape across modules** — see §7.

---

## 4. Hint discipline

### [HIGH] `validation_hinted` declared in `error.rs:287-297` — production callers: still **zero**

Re-verified at HEAD:
```
grep -n 'validation_hinted' crates/plugin-db/src/
crates/plugin-db/src/error.rs:287:    pub fn validation_hinted(
# (no other hits)
```

Carried verbatim from r3 §3. No production call site uses it. Every
coded-but-unhinted error in the table below could benefit from
converting to `validation_hinted(...)`. Pin: zero calls. Helper is dead
weight today.

### Gap-table re-walk at HEAD

| Code | Hint? | Site | Severity |
|---|---|---|---|
| `migration_cancelled` (mid-run) | NO | `migrations.rs:154-160` | MED — sibling on-start version has a thorough hint |
| `migration_not_cancellable` | NO | `migrations.rs:174-180` | MED — user calling cancel on a terminal run has no recovery advice |
| `invalid_argument` (multiple) | NO | `migrations.rs:223-234, 370-374, 462-464, 525-530, 535-540, 631-636` | LOW |
| `no_active_migration` | NO | `migrations.rs:378-383, 387-392, 460-464, 471-473` | LOW |
| `invalid_collection` | NO | `migrations.rs:222-228` | LOW |
| `migration_already_active` | NO | `migrations.rs:239-243` | LOW |
| `invalid_isolation_level` | NO | `orchestrator/transaction.rs:127-134` | MED — recovery answer in message body, not the `hint` slot |
| `tx_already_active` | NO | `orchestrator/transaction.rs:117-121` | LOW |
| `tx_settled` | NO | `v8_classes/transaction.rs:176-181` | LOW — POST-r3 ADDITION (new site) |
| `invalid_app_id` | NO | `audit.rs:823-838` | LOW |
| `lazy_init_failed` | NO | `register_model/mod.rs:119-123` | LOW |
| `backend_not_initialized` | NO | `register_model/mod.rs:125-128` | LOW |
| `not_configured` | NO | `error.rs:269-275` helper + every call site | MED — cold-start failure, the canonical "what do I do" moment |
| `From<QueryError>::invalid_filter`/`invalid_collection`/`invalid_identifier` | NO | `error.rs:351-364` | MED — `invalid_filter` benefits from a hint pointing at the filter catalog |
| `audit_bootstrap_failed` (catch-all only) | NO | `migrations.rs:121-130` | LOW |
| `cic_failed` (envelope carries `reason`) | NO | `backend/postgres.rs:413-424` | LOW — JSON body is the hint analog |
| `cic_configuration` (loop-exhaustion) | NO | `backend/postgres.rs:595-600` | LOW |
| `not_provisioned` | NO | `replication_ops.rs:230-234` | LOW |

### Post-r3 additions

One new coded site — `v8_classes/transaction.rs:175-181`:

```rust
if self.settled.get() {
    return Err(crate::error::DbError::validation(
        "tx_settled",
        "tx.collection: transaction already committed or rolled back",
    )
    .to_op_error());
}
```

This is the post-r3 cleanup move from `OpError::type_error` to typed
`DbError::validation` — a small improvement (`.code = "tx_settled"`
instead of opaque `TypeError`). But it's a textbook
`validation_hinted` candidate ("the wrapper is dead; open a new one
via `db.beginTransaction()`"). Re-listed in the table above.

`lock_guard.rs` (extracted by `cbd12944`) delegates to the underlying
`acquire_advisory_lock`'s typed `DbError`; no new coded site, no new
hint gap. Drop branch only logs (no JS-visible surface).

`subscription.rs` (post-`4cbe9fa1`) only adds `OpError::type_error` for
constructor-illegal / V8 alloc failures. Those aren't coded errors —
SDK branches on `instanceof TypeError`. No hint gap.

---

## 5. Retry semantics

### [MEDIUM] SDK default `withRetry` predicate matches **only** `optimistic_lock_failure`

Re-checked at HEAD. `sdks/db/src/with-retry.ts:25-30`:

```ts
export function isOptimisticLockError(e: unknown): boolean {
  return (
    e instanceof Error &&
    (e as { code?: unknown }).code === "optimistic_lock_failure"
  );
}
```

…and `withRetry`'s default `opts.on` is `isOptimisticLockError`.

**Why MEDIUM**: the native error rail now stamps `transient`,
`serialization_failure`, `lock_not_available` correctly (verified by
the `auto_tx.rs` tests + `error.rs::sql_violation_variants_stamp_canonical_codes`).
But the default `withRetry` predicate does NOT match any of these — SDK
users must hand-roll the predicate per the doc-comment example. Every
mutation under contention that could safely auto-retry (and the SDK
already correctly classifies as retryable via the `hint` field) falls
through to the user's catch handler instead.

This is the most visible "the native rail is doing the right thing but
the SDK isn't catching it" gap in the whole error UX. Cross-crate
scope (lives in `sdks/db/`), but worth flagging since the native
contract is now genuinely solid.

**Fix**: extend `isOptimisticLockError` (or add a sibling
`isRetryableError`) that ORs the three canonical retryable codes:

```ts
const RETRYABLE_CODES = new Set([
  "optimistic_lock_failure",
  "transient",
  "serialization_failure",
  "lock_not_available",
]);

export function isRetryableError(e: unknown): boolean {
  return (
    e instanceof Error &&
    typeof (e as { code?: unknown }).code === "string" &&
    RETRYABLE_CODES.has((e as { code: string }).code)
  );
}
```

…and switch `withRetry`'s default `opts.on` to `isRetryableError`. Or
better: stamp `e.retryable = true` natively on the three codes and
default the SDK to `e.retryable === true` (see §9 #5).

### [LOW] No native `auto_end_serialization_at_commit_preserves_code` test

Carried over from r3 §5. The conversion helper `end_to_resolve_value`
is shared; the existing test covers `LockContention`. A
`Serialization` test would cost ~10 LOC and pin the COMMIT-time path
explicitly. Low risk because the conversion routes through `to_op_error()`
verbatim; the `error.rs::retryable_variants_carry_hint` test already
pins the canonical mapping at the variant level.

### [LOW] No `retryable: bool` wire flag

Unchanged from r3 §5. The SDK still discriminates retryable codes by
maintaining its own predicate set. New retryable codes added natively
(e.g. `optimistic_lock_failure` exists, future
`replication_slot_not_provisioned` could be retryable) require a
predicate update in `sdks/db`. A boolean on `OpError::coded` would let
the SDK write `if (e.retryable)` once and never miss future additions.
Same reasoning, same impact: small today.

---

## 6. `SchemaRefused` payload shape

### [MEDIUM] `cic_failed` envelope `code` collision with `e.code` (unchanged from r3 §6)

Re-verified at HEAD (`backend/postgres.rs:413-424`):

```rust
let refuse = |value: serde_json::Value| -> DbError {
    let envelope_json = serde_json::to_string(&value).unwrap_or_else(|_| { … });
    DbError::SchemaRefused {
        code: "cic_failed",
        envelope_json,
    }
};
```

…and inside the envelope, the value's `"code"` field is one of:
- `"validation_refused"` — INVALID retry exhausted (line 492), non-transient build failure (line 574)
- `"unique_violation"` — data violation (line 533)

SDK sees `e.code = "cic_failed"` and `JSON.parse(e.message).code =
"validation_refused"`. Two `"code"` fields with two meanings in one
exception. A caller branching on `e.code === "validation_refused"`
will miss the CIC-validation path entirely.

**Why MEDIUM**: known gap, but it actively misroutes SDK error
handling for the CIC retry-exhaustion path.

**Fix**: rename the envelope-internal field to `"reason"`, OR align
`e.code` to the envelope-internal code and move the CIC origin marker
to `"source": "cic"`. The latter better matches the documented contract.

### [LOW] Envelope has no machine-typed `destructive_pending` schema

Same as r3 §6 second half. The SDK's `mapNativeError` returns the
error unchanged; consumers must `JSON.parse(e.message)` and walk
`destructive_pending` themselves. A typed `ValidationRefusedError`
subclass in `@zeroship/db` would let `mapNativeError` materialise the
violations / destructive list eagerly.

---

## 7. Mixed error shapes — JS-side consistency

### [HIGH] SDK `toJSON` STILL drops `hint` (unchanged from r3 §7)

`sdks/db/src/collection.ts:46-66` (verified at HEAD):

```ts
Object.defineProperty(out, "toJSON", {
  value: function () {
    const obj: Record<string, unknown> = {
      name: (this as Error).name,
      message: (this as Error).message,
    };
    const code = (this as { code?: unknown }).code;
    if (code !== undefined) obj.code = code;
    const errs = (this as { errors?: unknown }).errors;
    if (errs !== undefined) obj.errors = errs;
    return obj;
  },
  …
});
```

Still no `hint` propagation. Every retryable code's actionable
recovery advice (correctly stamped at native via `8ff1b2de`) silently
disappears at the RPC `JSON.stringify` boundary. The 3-LOC fix r3
ranked #1 by SDK-author impact; still **not landed**.

**Why HIGH (carried over)**: this is the single highest-impact
unaddressed item. Every other hint improvement in the native layer is
invisible past the RPC boundary until this lands.

**Fix**: 3 lines added to `sdks/db/src/collection.ts`:
```ts
const hint = (this as { hint?: unknown }).hint;
if (hint !== undefined) obj.hint = hint;
```

### [MEDIUM] `mapNativeError` short-circuits on `.code` but never inspects `.hint` (unchanged from r3 §7)

`sdks/db/src/errors.ts:63-69` (verified at HEAD):

```ts
export function mapNativeError(e: unknown): Error {
  if (e instanceof Error && typeof (e as { code?: unknown }).code === "string") {
    return e;
  }
  …
}
```

Pass-through when `.code` is a string, but never copies / serialises
`e.hint`. Same root cause as the `toJSON` gap; the fix is to ensure
both paths preserve `hint` end-to-end.

### [MEDIUM] Inconsistent error-source prefix across `migrations` / `audit` / `register_model`

Re-checked at HEAD (after `dec2bd42`):

- `audit.rs::coded_sql` → `"audit: <ctx>: <pg-msg>"` where `<pg-msg>`
  already starts with `"db: "` → compound shape `"audit: insert: db: <real>"`.
- `migrations.rs::coded_db` → `"<ctx>: <pg-msg>"` (single prefix now,
  correctly).
- `migrations.rs::coded("audit_bootstrap_failed", "audit bootstrap failed: <body>")`
  via `map_audit_bootstrap_err` — different shape again.
- `walk_pg_chain` (`error.rs:371-379`) prepends `"db: "` to every
  Postgres error.

For the SDK consumer the impact is minor (they branch on `.code`).
For the operator reading worker logs, the inconsistency is noise: `"audit:
create_idx: db: <real msg>"` and `"migration insert: db: <real msg>"`
co-exist.

**Fix**: remove the `"db: "` prefix from `walk_pg_chain` since every
downstream wrapper already adds context; OR drop both `"audit: "` and
`"db: "` prose prefixes entirely since `.code` already encodes the
subsystem.

### Removed: §7 r3 `coded_db` double-prefix item

r3 §7 listed this as a REGRESSION. Closed by `dec2bd42`. Pinned at
`migrations.rs:93-97` with an inline comment citing the revert
history. Verified at HEAD:

```rust
// `message` already starts with "db: " from `walk_pg_chain`;
// prepend only the lifecycle context phrase to avoid the
// doubly-prefixed "db: {context} failed: db: ..." output.
// Restored from 60ca1ad6 — silently reverted by ed697c45.
*message = format!("{context}: {message}");
```

---

## 8. `DbError::Coded` variant remains production-unused (unchanged from r3 §8)

Re-checked at HEAD:

```
grep -n 'DbError::Coded' crates/plugin-db/src/
crates/plugin-db/src/error.rs:40://! | [`DbError::Coded`] | … |
crates/plugin-db/src/error.rs:239:            DbError::Coded { code, message, hint } => OpError::coded(code, message, hint),
crates/plugin-db/src/error.rs:264:            | DbError::Coded { message, .. }
crates/plugin-db/src/error.rs:327:            | DbError::Coded { message, .. }
crates/plugin-db/src/error.rs:449:        let e = DbError::Coded {
```

Variant declaration, impl arms, one test. Every migrations lifecycle
code (`migration_already_running`, `migration_cancelled`, etc.) still
builds an `OpError` directly via `coded(...)`, bypassing the
`DbError::Coded` envelope. Neutral, not a regression. Either start
using it (route `migrations::coded()` through `DbError::Coded →
to_op_error`) or delete the variant + its test.

---

## 9. OpError envelope shape — consistency

Re-checked every `ResolveValue::RejectError(...)` site (16 across
`crud.rs`, `replication_ops.rs`, `auto_tx.rs`, `register_model/mod.rs`,
`orchestrator/transaction.rs`). Every one materialises through
`to_op_error()` → `OpError::CodedError { code, hint }` (or
`TypeError` for arg-validation refusals at the v8_class entry, which
are not coded by design — SDK uses `instanceof TypeError`).

Envelope shape is uniform. The only inconsistency is the wrap-as-Internal
flatten in `replication_ops.rs` (§1.1).

---

## 10. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC est. | Files | Status vs r3 |
|---|---|---|---|---|
| 1 | §7 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts` | UNCHANGED (r3 rank 1) |
| 2 | §1.1 — `replication.rs` returns `Result<_, String>`; 4 dispatch sites flatten | ~30 | `replication.rs`, `replication_ops.rs` | UNCHANGED (r3 rank 4) — bumped to top-tier; r3 regressions are now closed |
| 3 | §5 — SDK `withRetry` default predicate matches only `optimistic_lock_failure`; auto-retry of `transient` / `lock_not_available` / `serialization_failure` requires hand-rolling | ~15 | `sdks/db/src/with-retry.ts` | NEW — surfaced this round because the native rail is now stable enough for the gap to dominate |
| 4 | §4 — add hints to `migration_cancelled` (mid-run), `migration_not_cancellable`, `invalid_isolation_level`, `not_configured`, `From<QueryError>::invalid_filter`, `tx_settled` | ~25 | `migrations.rs`, `transaction.rs`, `error.rs`, `v8_classes/transaction.rs` | UNCHANGED (+1 NEW site: `tx_settled`) |
| 5 | §5 — stamp `retryable: true` on `OpError::coded` for `transient` / `serialization_failure` / `lock_not_available`; SDK branches on `e.retryable` | ~15 | `error.rs` + `state.rs` | UNCHANGED |
| 6 | §6 — `cic_failed` envelope `code` collides with `e.code` | ~10 | `backend/postgres.rs:413-424` + tests | UNCHANGED |
| 7 | §5 — add `auto_end_serialization_at_commit_preserves_code` test | ~10 | `auto_tx.rs` tests | UNCHANGED |
| 8 | §3 — augment "INSERT returned no row" messages with `(app_id, collection, name)` triple | ~10 | `audit.rs:329,617`, `migrations.rs:329` | UNCHANGED |
| 9 | §4 — actually use `DbError::validation_hinted` (production count: 0) | ~15 sites | scattered | UNCHANGED |
| 10 | §8 — either use `DbError::Coded` (route `migrations::coded()` through it) or delete the variant | ~30 sites OR ~5 LOC delete | `migrations.rs`, `error.rs` | UNCHANGED |

---

## 11. Score

**77 / 100** (+6 vs r3's 71)

**What earned the +6:**

- r3 §1.1 HIGH (`tx_connect_failed`) and r3 §7 HIGH (`coded_db`
  double-prefix) both closed by `dec2bd42` with inline comments citing
  the revert. The fix isn't just "edit lines back" — it's also a code
  trail that makes a future silent revert visible at the diff hunk.
  Real +5.
- r3 §6 doc lie about `SchemaRefused.code` closed by `07205e54`.
  Real +1. (The doc was wrong; users reading it would have been
  misled about the wire contract.)
- Subscription leak fix (`4cbe9fa1`) is correct and the error path
  there is type-error-only (no coded contract change). The fix moves
  the broker subscribe BELOW the V8 alloc so a `?`-propagated alloc
  failure can't leak a broker entry. Doesn't move the score, but
  no error UX regression introduced. Neutral.
- Backfill progress before COMMIT (`37e61803`) is a correctness fix
  (reset-clobber race); errors on the path use `coded_db` like
  everything else. Neutral for this lens.

**Why not higher (the -23 deficit):**

- `sdks/db/src/collection.ts` `toJSON` still drops `hint` — this is a
  3-LOC fix that has slipped through r2 / r3 / r4 cycles. Every other
  hint improvement in the native layer is invisible past the RPC
  boundary until it lands.
- `replication.rs` rail still on `Result<_, String>`, four dispatch
  sites still flatten to `DbError::Internal`. The only remaining native
  `.code` discipline gap; promoted to HIGH this round because every
  other regression is closed.
- `validation_hinted` and `DbError::Coded` continue to be declared
  helpers with zero production callers. Either route the existing
  migrations lifecycle codes through them or delete them.
- No `retryable: bool` wire flag yet; SDK predicate maintenance is
  manual.

**What the trajectory looks like:**

r2 → r3 → r4 is a steady +4, +6 climb. The regressions in r3 were a
process problem (silent revert during a refactor); the response in r4
included inline source-history comments that make the same mistake
harder to repeat. The remaining gaps are concrete and quantified —
each line in the rank table above is a 1–30 LOC fix with no
architectural ambiguity.

**Projected next-round ceiling:**

- §10 #1 (SDK toJSON hint, 3 LOC) → +5
- §10 #2 (replication.rs to typed errors, ~30 LOC) → +5
- §10 #3 (SDK withRetry default predicate, ~15 LOC) → +3
- §10 #4 + #5 + #9 partial (hints + `retryable: true` + start using
  `validation_hinted` at 4-5 obvious sites, ~40 LOC) → +3

If all of the above land cleanly, the next score should be in the
93/100 range. The remaining 7 are reserved for the public error-codes
catalog doc (`docs/reference/db-error-codes.md`) and a typed
`ValidationRefusedError` SDK subclass — both medium-effort and not yet
on the backlog.
