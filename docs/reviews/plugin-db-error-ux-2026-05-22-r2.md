# plugin-db Error-UX Review — 2026-05-22 r2

Scope: `crates/plugin-db/src/` at HEAD (post `309ed52f`, `ed697c45`,
`60ca1ad6`, `b94fbdeb`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r1.md`.

Lens: SDK-author error-handling discipline. Every finding evaluates the
JS-visible surface — `e.code`, `e.message`, `e.hint`, `e.cause` — and
whether the SDK can branch on it without parsing strings.

---

## TL;DR (changes since r1)

Resolved since r1:
- **r1 §4b** (`audit_bootstrap_failed` Debug formatting) — fixed by the
  new `map_audit_bootstrap_err` helper
  (`crates/plugin-db/src/migrations.rs:117-126`). `Transient`,
  `LockContention`, etc. now flow through `to_op_error()` verbatim;
  only the `Internal` catch-all gets re-wrapped. Two new regression
  tests pin the behaviour (`migrations.rs:868-908`).
- **r1 §4c** (`tx_connect_failed` discards SQLSTATE) — *partially*
  fixed. The single remaining site at `migrations.rs:255-258` still
  uses `coded("tx_connect_failed", &e.into_string(), None)` and
  flattens any `Transient` into a non-classified code. The orchestrator
  / register_model path is gone, but `exec_begin` (the migrations
  lifecycle) still carries the bug.
- **r1 §4d** (`AddIndex` wraps as `Internal`) — fixed. `apply.rs:135`
  now calls `create_index_with_recovery` whose typed `DbError` flows
  through verbatim, including new `SchemaRefused { code: "cic_failed" }`
  envelopes. See new finding §5 for a fresh inconsistency this
  introduced.
- **r1 §4a** (`SchemaRefused` strips `.code`) — fixed. `to_op_error()`
  now stamps the static `code` onto the JS exception
  (`error.rs:191-200`); tests pin both the code and the envelope shape
  (`error.rs:420-440`). `validation_refused` is now discoverable via
  `e.code === "validation_refused"`.

Net score uplift vs r1: see §10.

---

## 1. `.code` discipline

### [HIGH] `crates/plugin-db/src/orchestrator/auto_tx.rs:67,104` — auto-tx still flattens DbError via `OpResult::Failed { error: String }`

```rust
// auto_tx.rs:60-72 (exec_auto_begin → OpResult)
match exec_auto_begin(...).await {
    Ok(token) => OpResult::Completed { ... },
    Err(e) => OpResult::Failed {
        op_id,
        error: e.into_string(),   // <-- flattens DbError to a String
        request_id,
    },
}
```

The runtime side (`crates/runtime/src/core/dispatch.rs:399-402`) builds
a plain `v8::Exception::error(scope, msg)` — i.e. a `new Error(msg)`
with **no** `.code` property. A `Transient` from
`exec_auto_begin`'s `connect` (line 174-178) reaches JS as an Error with
no `e.code`, no `e.hint`, just a string body. The same flattening
happens in `auto_end_transaction`'s `exec_auto_end` (which calls
`DbError::from_pg(&e)` then immediately throws away the classification).

**Why**: SDK callers wrapping `query()`/`mutation()` handlers cannot
write `if (e.code === "transient") retry()` against an auto-tx
commit-time failure. They also lose the retry hint
(`"transient backend failure; retry after a short backoff"`).
Particularly bad because COMMIT failures during auto-tx are the path
where the SDK most needs to surface a typed code — the user's handler
already succeeded, and the only thing left is the platform's COMMIT.

**Fix**: drop `OpResult::Failed` here entirely and resolve through the
existing `setup_promise` resolver with `ResolveValue::RejectError(e.to_op_error())`,
matching the pattern in `crud.rs:run_op` and
`orchestrator/transaction.rs:88-99`. Concretely: extend
`setup_promise` to return a `v8::Global<v8::PromiseResolver>` (instead
of plumbing through `op_id`), then switch both `auto_begin_transaction`
and `auto_end_transaction` to `OpResult::JsValue { resolver, value:
ResolveValue::RejectError(e.to_op_error()), ... }`. The conversion
will preserve `.code` and `.hint`.

**Verification**:
```
grep -n 'OpResult::Failed' crates/plugin-db/src/
```
→ exactly two hits, both in `auto_tx.rs`. Every other dispatch path
(`crud.rs`, `orchestrator/transaction.rs`, `v8_classes/migrations.rs`,
`v8_classes/migration.rs`, `register_model/mod.rs`) uses
`ResolveValue::RejectError(e.to_op_error())`.

---

### [HIGH] `migrations.rs:255-258` — `tx_connect_failed` discards SQLSTATE

```rust
let client = backend
    .acquire_dedicated_client()
    .await
    .map_err(|e| coded("tx_connect_failed", &e.into_string(), None))?;
```

`e.into_string()` strips the `DbError` variant — if the connection
failure was a `Transient` (the common case: pool exhausted, postgres
restarting), the SDK sees `.code = "tx_connect_failed"` instead of
`.code = "transient"`. The standard SDK retry policy (any predicate
checking `e.code === "transient"`) skips this case. The actionable
`"transient backend failure; retry after a short backoff"` hint also
disappears.

**Why**: `tx_connect_failed` is operator-shaped; `transient` is
SDK-shaped. The SDK retry layer cannot retry on `tx_connect_failed`
without hard-coding a special case.

**Fix**: replace with the same `map_audit_bootstrap_err`-style
pattern — preserve SQLSTATE-coded variants verbatim, only re-wrap
`Internal`:
```rust
.map_err(|e| match e {
    DbError::Internal { message } => coded(
        "tx_connect_failed",
        &format!("tx connect failed: {message}"), None,
    ),
    other => other.to_op_error(),
})?;
```

**Verification**: `grep -n 'tx_connect_failed' crates/plugin-db/src/` →
the single site in `migrations.rs:258`.

---

### [MEDIUM] `migrations.rs:317-323` — manual `coded("internal", ...)` instead of `DbError::Internal`

```rust
if id == 0 {
    return Err(coded(
        "internal",
        "db: migration insert returned no row",
        None,
    ));
}
```

The two other `audit: ... returned no row` sites
(`audit.rs:328-330`, `audit.rs:887-889`) build a `DbError::Internal`
that flows through `coded_db`'s prefix path — yielding a consistent
`"db: migration insert failed: audit: insert_backfill_running returned
no row"` chain. This direct `coded("internal", ...)` call bypasses
that pipeline and emits `"db: migration insert returned no row"`. The
*code* matches (`"internal"`), but the *shape* of the message differs
from sibling audit failures.

**Why**: minor consistency issue; an operator scanning logs sees one
message starting with `"db: migration insert failed:"` (from
`coded_db`) and another starting with `"db: migration insert returned
no row"` (from this direct call) — they reference the same SQL
operation but look like unrelated failures.

**Fix**: either route through `DbError::Internal { ... }` + `coded_db`,
or align the prefix here to match.

---

## 2. Error message quality

Random sample of 10 `DbError::Internal { message: format!(...) }` or
`coded("internal", ...)` sites — rating against the criterion "an
operator can diagnose this without opening the source":

| Message | Source | Rating | Notes |
|---|---|---|---|
| `"db: migration insert returned no row"` | `migrations.rs:320` | POOR | What was the INSERT? Which table, which `app_id`/`name`/`collection`? The SDK / operator has zero handles to find the failing run. |
| `"db: apply received a {kind} op outside ChangeClass::Destructive (class={:?}); upstream destructive-class filter (register_model::apply pass1/pass2) should have skipped it. This is a contract violation — refusing to silently no-op."` | `apply.rs:282-289` | GOOD | Names the breached invariant, the path that should have caught it, and the operator-facing impact. Verbose but the right kind of verbose. |
| `"db: lazy init failed: {e}"` | `orchestrator/register_model/mod.rs:121` | POOR | The inner `{e}` is `init_pool_async`'s raw error. If the pool failure includes a connection URL fragment, that leaks into the JS exception (Postgres URLs commonly carry secrets). |
| `"db: backend not initialized"` | `register_model/mod.rs:127` | OK | Clear but not actionable — what should the operator do? Hint should advise the lazy-init path or capability-gate check. |
| `"db: pg_advisory_lock failed: {message}"` | `register_model/bootstrap.rs:116` | OK | States the operation; the inner `{message}` is `walk_pg_chain`'s output which is reasonably structured. |
| `"db: create schema failed: {message}"` | `register_model/bootstrap.rs:146` | OK | Same shape. |
| `"db: transaction connection lost"` | `exec.rs:52` | POOR | This is the take-from-empty-slot path inside `run_sql` — it means an in-flight CRUD found `tx_conn` empty even though `has_tx()` returned true. Almost always indicates the auto-tx slot was stolen by a re-entrant call. The message doesn't name the kind of tx, the collection, or the suggested recovery. |
| `"db: not configured"` | `exec.rs:64`, `orchestrator/auto_tx.rs:173`, `orchestrator/transaction.rs:146` | OK | Code is `not_configured`; clear enough. Could carry a hint like `"set DB_URL or run via control-plane bootstrap"`. |
| `"db: create index '{name}' exhausted retry budget without a terminal result"` | `backend/postgres.rs:597-600` | GOOD | Names the index, the contract (exhaustion without terminal), and is clearly internal. Code is `cic_configuration`. |
| `"db: invalid collection: {e}"` | `migrations.rs:221` | OK | Embeds the inner `validate_collection` error. The `"db: "` prefix here is double work: `coded("invalid_collection", &format!("db: invalid collection: {e}"), None)` and the underlying `e` already starts with `"collection name "` etc. Net: `"db: invalid collection: collection name '...' must..."` — readable but redundant. |

### Recurring offenders

1. **`"INSERT returned no row"` / `"insert_backfill_running returned no row"`** (`audit.rs:329`, `audit.rs:617`, `migrations.rs:320`) — Three sites with this shape. All three are the same defensive guard ("RETURNING `id` was empty"). None carry the `(app_id, collection, name)` triple that would let an operator locate the failing run. The code is `internal` so SDK can't branch.

2. **Inner-error leakage**. `"db: lazy init failed: {e}"` and `"db: auto-tx connect failed: {e}"` (`auto_tx.rs:177`) and `"db: tx connect failed: {e}"` (`orchestrator/transaction.rs:150`) all interpolate `init_pool_async` / `compio_postgres::Error`'s `Display`. Sensitive material (connection URLs, DSN host) ends up in `e.message` reachable from the user's browser console.

---

## 3. Hint discipline

### [MEDIUM] Audit table: only retryable SQLSTATE variants carry hints

Sweep of `coded(...)` / `DbError::*` sites with a hint:

| Code | Hint? | Site |
|---|---|---|
| `serialization_failure` | YES | `error.rs:216-220` |
| `transient` | YES | `error.rs:226-230` |
| `lock_not_available` | YES | `error.rs:221-225` |
| `migration_already_running` | YES | `migrations.rs:128-137` |
| `migration_cancelled` (on-start) | YES | `migrations.rs:139-148` |
| `migration_reset_externally` | YES | `migrations.rs:158-168` |
| `migration_not_active` | YES | `migrations.rs:182-191` |
| `migration_cancelled` (mid-run) | NO | `migrations.rs:150-156` |
| `migration_not_cancellable` | NO | `migrations.rs:170-176` |
| `invalid_argument` (any) | NO | `migrations.rs:226-231`, `:361-365`, `:458-460`, `:516-520`, `:525-530`, `:608-616` |
| `no_active_migration` | NO | `migrations.rs:369-374`, `:377-383`, `:451-455`, `:462-464` |
| `invalid_collection` | NO | `migrations.rs:218-224` |
| `migration_already_active` | NO | `migrations.rs:226-231` |
| `invalid_isolation_level` | NO | `orchestrator/transaction.rs:128-134` |
| `tx_already_active` | NO | `orchestrator/transaction.rs:117-121` |
| `invalid_app_id` | NO | `audit.rs:823-838` |
| `lazy_init_failed` | NO | `orchestrator/register_model/mod.rs:119-123` |
| `backend_not_initialized` | NO | `register_model/mod.rs:125-128` |
| `not_configured` | NO | `error.rs:265-270` (helper), all call sites |
| `invalid_filter` / `invalid_collection` / `invalid_identifier` (from `From<QueryError>`) | NO | `error.rs:346-360` |
| `tx_connect_failed` | NO | `migrations.rs:255-258` |
| `audit_bootstrap_failed` (catch-all only) | NO | `migrations.rs:119-123` |

**Specific gaps to fix:**

1. **`migration_cancelled` (mid-run) has no hint** despite the sibling
   on-start version having a thorough recovery hint. The mid-run case
   tells the user `"migration was cancelled by an operator"` with no
   guidance — should say "this run has been cancelled; mint a fresh
   wrapper via `env.db.migrations.start({reset: true})` to resume" or
   similar. (`migrations.rs:150-156`)

2. **`migration_not_cancellable` has no hint** — a user calling
   `.cancel()` on an already-terminal migration sees `"migration in
   state 'applied' cannot be cancelled"` and has no idea what to do
   instead (e.g. "use reset() to retry from scratch").

3. **`From<QueryError>` collapses three codes onto bare
   `ValidationFailed { hint: None }`** (`error.rs:354-358`). The
   `invalid_filter` path in particular benefits from a hint pointing
   at the schema doc — `"Allowed operators: $eq, $ne, $gt, ..."` or
   even a stable URL.

4. **`invalid_isolation_level`** has the *answer* inline in the message
   (`"Must be one of: read uncommitted, read committed, repeatable
   read, serializable"`) but as message body, not `e.hint`. The SDK
   pattern is to display `hint` as recovery advice; users get nothing
   structured here.

5. **`tx_connect_failed`** is operator-actionable (check pool, check
   postgres health) but carries no hint.

6. **`not_configured`** is the cold-start failure mode — should hint
   `"set DB_URL in env or wait for control-plane bootstrap"`.

**Fix**: invoke `DbError::validation_hinted(...)` in all the above
sites (the helper exists at `error.rs:282-292` but is **never used in
production code** — `grep -n validation_hinted` shows only the
declaration). Adding hints to even 5 of the gaps above would
meaningfully improve the "creator hits an error in console" path.

**Verification**:
```
grep -nE 'validation\(|validation_hinted\(|coded\(' crates/plugin-db/src/migrations.rs | wc -l
# 24 sites; most call `coded(...)` with `None` for the hint.
```

---

## 4. Retry semantics

### [LOW] No `retryable: bool` flag on the wire; `Transient` is the only retry contract

`DbError::Transient { message }` (`error.rs:106-108`) does not carry a
boolean `retryable` field. The implicit contract is "any error whose
code reaches JS as `"transient"` / `"serialization_failure"` /
`"lock_not_available"` is retryable". The `e.hint` says so in prose:

```rust
DbError::Transient { ... } => OpError::coded(
    "transient",
    message,
    Some("transient backend failure; retry after a short backoff".to_string()),
),
```

`DispatchResult::ErrorValue` (in
`crates/runtime/src/core/state.rs:885-893`) **does** have a
`retryable: Option<bool>` field, but only for user-thrown `HttpError`
objects (read from `err.retryable` in `init.rs`). Native plugin-db
errors do not set this; the SDK has no `e.retryable` to consult.

**Why this is small**: the SDK's `withRetry` helper
(`sdks/db/src/with-retry.ts:25-30`) already gates on `e.code` which is
a perfectly adequate contract — the user composes `e.code ===
"serialization_failure"` etc. There's no functional gap.

**Why it might matter later**: adding an `optimistic_lock_failure` /
`unique_violation_with_retry` type later means the SDK has to add new
strings to its predicate. A boolean flag (`retryable: true` on
`Transient` / `Serialization` / `LockContention` / `OptimisticLockError`)
would let the SDK write `if (e.retryable) ...` and never miss a future
addition. This is the same convention the gateway uses for upstream
errors.

**Fix (optional)**: extend `OpError::coded` (or add `OpError::coded_retryable`)
so the runtime can stamp `e.retryable = true` on the JS exception.
Then have `to_op_error()` for the three retryable variants set it.

### [MEDIUM] `LockContention` and `Serialization` use the same hint shape but distinct codes — fine; auto_tx never lets the SDK observe COMMIT-time `Transient` (per §1)

The COMMIT path in `auto_tx.rs:exec_auto_end:232-248` correctly routes
through `DbError::from_pg(&e)` for the COMMIT statement — but the
resulting typed error is then flattened in `auto_end_transaction:104`
(see §1). So the retry-by-`.code` contract is broken at COMMIT time.
This is the highest-impact retry-semantics gap: the user's
`mutation()` handler succeeded; a `Transient` at COMMIT is exactly
when retry is most beneficial, and exactly when the SDK can't see it.

---

## 5. SchemaRefused payload shape

### [MEDIUM] `cic_failed` `SchemaRefused` carries `code: "cic_failed"` on the JS exception but a different `code` inside the envelope

`crates/plugin-db/src/backend/postgres.rs:413-424`:
```rust
let refuse = |value: serde_json::Value| -> DbError {
    let envelope_json = serde_json::to_string(&value)...;
    DbError::SchemaRefused {
        code: "cic_failed",          // <-- JS-visible e.code
        envelope_json,
    }
};
```

But the envelope JSON's *internal* `"code"` field is built per
case (lines 492, 533, 574) and is one of:
- `"validation_refused"` for index-landed-INVALID (line 492) or
  non-transient failure (line 574)
- `"unique_violation"` for data-violation paths (line 533)

So an SDK callback that does:
```ts
catch (e) {
    if (e.code === "validation_refused") {
        const { violations } = JSON.parse(e.message);
        // ...
    }
}
```
would NOT match a CIC-validation refusal even though the envelope body
says `"validation_refused"`. The SDK must also branch on `e.code ===
"cic_failed"`, then `JSON.parse(e.message)` to inspect the inner code.

**Why**: the JS-side `.code` and the envelope-internal `.code` mean
different things now — the outer is "where the refusal originated",
the inner is "what the SDK should rendered to the user". Both have
the name `"code"`, easy to confuse.

**Fix options**:
1. Rename the envelope-internal field to `"reason"` so it's
   distinguishable from `e.code`.
2. Keep the envelope-internal `"code"` and make `e.code` equal it (so
   CIC-validation-refused is `e.code === "validation_refused"` exactly
   like a regular validate-stage refusal). The cost: operators lose
   the "this came from CIC vs validate stage" classification — move
   that to a `"source": "cic"` field inside the envelope.

Option 2 better matches the SDK's existing contract ("branch on
`e.code`, parse `e.message` for details"). Option 1 is the smaller
change.

**Verification**:
```
grep -n '"cic_failed"\|"validation_refused"' crates/plugin-db/src/backend/postgres.rs
```
→ shows the two distinct meanings of "code" in the same source file.

### [LOW] `SchemaRefused` envelope has no machine-readable `field` / `collection` index

The envelope structure is documented in `validate.rs:128-134`:
```json
{
  "code": "validation_refused",
  "deploy_id": "...",
  "violations": [],
  "destructive_pending": [
    { "collection": "...", "change_kind": "...", "field": "...", "details": {...} }
  ]
}
```

The SDK's `mapNativeError` (`sdks/db/src/errors.ts:67-69`) returns the
error unchanged. Consumers must `JSON.parse(e.message)` and then walk
`destructive_pending` themselves. No SDK helper renders this. The
field `details` is opaque (a `serde_json::Value` produced by
`diff::DiffOp::details` — varies per `change_kind`).

This isn't a regression vs r1 but it's a real consumer gap: the SDK
should ship a `ValidationRefusedError` subclass with a typed
`destructivePending: Array<{collection, changeKind, field, details}>`
property, parsed eagerly in `mapNativeError`.

---

## 6. `OpResult::Failed { error: String }` sweep

The only two reachable sites in plugin-db are
`orchestrator/auto_tx.rs:67,104` (covered in §1). Every other path uses
`OpResult::JsValue { resolver, value: ResolveValue::RejectError(e.to_op_error()), ... }`
which preserves `e.code` and `e.hint`.

```
grep -n 'OpResult::Failed' crates/plugin-db/src/
crates/plugin-db/src/orchestrator/auto_tx.rs:67
crates/plugin-db/src/orchestrator/auto_tx.rs:104
```

Once §1 is fixed, this entire shape can be removed from plugin-db.

---

## 7. Mixed error shapes — `e.code`, `e.message`, `e.cause` consistency

### [HIGH] `hint` is set on the JS Error but **dropped by the SDK's `toJSON`**

`sdks/db/src/collection.ts:33-67` defines `toResultError` which patches
a `toJSON` onto the Error so it survives RPC serialization:
```ts
const obj: Record<string, unknown> = {
    name: (this as Error).name,
    message: (this as Error).message,
};
const code = (this as { code?: unknown }).code;
if (code !== undefined) obj.code = code;
const errs = (this as { errors?: unknown }).errors;
if (errs !== undefined) obj.errors = errs;
```

`hint` is **not** copied. The runtime's coded-error helper carefully
sets `e.hint = hint` on the JS exception
(`runtime/src/core/runtime.rs:2101-2112`); the native side spends real
effort building the hint text — but at the RPC boundary
(server→client over `_zs/v1/<id>`) the hint is silently dropped.

**Why this is high-impact**: every retryable error
(`serialization_failure`, `transient`, `lock_not_available`,
`migration_*`) has its actionable hint stripped before reaching the
client. The very feature `error.rs` advertises in tests
(`retryable_variants_carry_hint`) is invisible to users.

**Fix**: extend the `toJSON` patch:
```ts
const hint = (this as { hint?: unknown }).hint;
if (hint !== undefined) obj.hint = hint;
```

Same for `cause` if anyone surfaces it from native (none currently
does, but `mapNativeError`'s back-compat `new Error(msg, { cause: e })`
path at `errors.ts:73` builds a cause that this `toJSON` patch drops).

**Verification**:
```
grep -n 'toJSON' sdks/db/src/collection.ts
```
→ the toJSON literal field set is `name`/`message`/`code`/`errors`.
No `hint`.

### [MEDIUM] `mapNativeError` short-circuits on `.code` string but never inspects `.hint` or `.cause`

`sdks/db/src/errors.ts:67-69`:
```ts
if (e instanceof Error && typeof (e as { code?: unknown }).code === "string") {
    return e;   // <-- passes through, but never inspects e.hint
}
```

Combined with the `toJSON` gap above, this means hints exist only on
the same isolate where the error was thrown. A handler that returns
the error structure across the RPC boundary loses them.

### [MEDIUM] Inconsistent error-source prefix across migrations / audit / register_model

- `audit.rs` calls produce `"audit: <ctx>: <pg-message>"` (e.g.
  `"audit: create __zeroship_migrations: db: ..."`)
- `migrations.rs` coded_db produces `"db: <ctx> failed: <pg-message>"`
- `walk_pg_chain` already prepends `"db: "` to the pg message
- Net: `"audit: create_idx: db: <real msg>"` and `"db: migration insert
  failed: db: <real msg>"` — the `"db: "` prefix from `walk_pg_chain`
  is repeated in the operator-visible string (r1 §4e is still
  present; only the apply.rs ADD COLUMN site was fixed to suppress the
  redundant prefix).

For the SDK consumer the impact is minor (they branch on `e.code`).
For the operator reading worker logs the noise compounds: `audit:`,
`db:`, and `db:` again all in one message.

**Fix**: remove the `"db: "` prefix from `walk_pg_chain` since every
downstream wrapper already adds its own context. Better yet: drop the
`"audit: "` and `"db: "` prefixes entirely — the `.code` already
identifies the subsystem (`unique_violation`, `tx_connect_failed`,
`audit_bootstrap_failed`), the prose duplicates it.

**Verification**:
```
grep -nE '"db: |"audit: ' crates/plugin-db/src/ -r | wc -l
# ~30 sites; most are the redundant prefix.
```

---

## 8. New variant: `Coded` passthrough — undocumented in r1, neutral

`DbError::Coded { code, message, hint }` (`error.rs:121-125`) was
introduced after r1 to let migrations-lifecycle codes
(`migration_already_running`, `migration_reset_externally`, …) flow
through generic helpers without being flattened. The test at
`error.rs:442-457` pins the passthrough.

In practice this is *unused* in the codebase — `grep -n
'DbError::Coded' crates/plugin-db/src/` shows only the variant
declaration, the impl arms, and one test. Every coded migration error
is built via `migrations::coded(...)` which directly produces an
`OpError`, bypassing `DbError`. The variant exists as a future-proof
hook but no caller benefits from it today.

Not a regression. Worth flagging because the doc comment promises a
benefit that no production site reaps; either start using it (route
migrations.rs's `coded()` results through `DbError::Coded → to_op_error`)
or delete the variant + helpers.

---

## 9. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC est. | Files |
|---|---|---|---|
| 1 | §7 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts` |
| 2 | §1 — auto_tx flattens DbError | ~30 | `orchestrator/auto_tx.rs` |
| 3 | §2 — `tx_connect_failed` discards SQLSTATE | ~6 | `migrations.rs:255-258` |
| 4 | §3 — add hints to `migration_cancelled` (mid-run), `migration_not_cancellable`, `invalid_isolation_level`, `not_configured` | ~20 | `migrations.rs`, `orchestrator/transaction.rs`, `error.rs` |
| 5 | §5 — `cic_failed` envelope `code` collision with `e.code` | ~10 | `backend/postgres.rs:413-424` + tests |
| 6 | §2 — strip the `"db: "` redundant prefix from `walk_pg_chain` (cascade cleanup; touches operator logs only) | ~5 | `error.rs:366-374` |
| 7 | §7 — document the SDK should also propagate `cause` via `toJSON` | ~3 | `sdks/db/src/collection.ts` |
| 8 | §2 — augment "INSERT returned no row" messages with the (app_id, collection, name) triple | ~10 | `audit.rs:329`, `audit.rs:617`, `migrations.rs:320` |
| 9 | §3 — actually use `DbError::validation_hinted` (production count: 0) | ~15 sites | scattered |

---

## 10. Score

**67 / 100** (+9 vs r1's 58)

Three of r1's top three findings are resolved (`audit_bootstrap_failed`
Debug formatting, `SchemaRefused` strips `.code`, `AddIndex` wraps as
Internal). The remaining gaps are smaller in number but more
SDK-shaped:

- §1 (`OpResult::Failed { error: String }` in auto_tx) is the single
  highest-impact remaining gap — it breaks `.code` and `.hint` on the
  COMMIT-time error path where retry-by-code matters most.
- §7 (SDK `toJSON` drops `hint`) renders the entire `hint` discipline
  invisible past the RPC boundary; this is essentially a 3-LOC fix
  with outsized payoff.
- §5 (`cic_failed` vs envelope `validation_refused` code collision)
  is a new inconsistency introduced by the `AddIndex` typed-error fix.

The retryable-with-hint design is genuinely good. The `.code` catalog
remains stable. `From<compio_postgres::Error>` and
`From<QueryError>` keep all classification in one place. The
`map_audit_bootstrap_err` test-driven discipline is a model for how
the remaining `tx_connect_failed` site should be fixed.

If §1, §7-§1, and §3's top 4 hint additions ship, this would land at
~80/100. The remaining 20 points are reserved for: documented public
code catalog (`docs/reference/db-error-codes.md` — still missing,
flagged in r1 §2 notes), structured `details` on `SchemaRefused`
(§5 second half), and the longer-term `retryable: bool` wire field
(§4).
