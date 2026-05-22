# plugin-db Error-UX Review — 2026-05-22 r3

Scope: `crates/plugin-db/src/` at HEAD (post `8ff1b2de`, `cbd12944`,
`c83d6a8c`, `ed697c45`, `43ac6c8e`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r2.md`.

Lens: SDK-author error-handling discipline. Every finding evaluates the
JS-visible surface — `e.code`, `e.message`, `e.hint`, `e.cause` — and
whether the SDK can branch on it without parsing strings.

---

## TL;DR (changes since r2)

Resolved since r2:

- **r2 §1 HIGH** (`auto_tx.rs` flattened `DbError` via
  `OpResult::Failed { error: String }`) — **closed by 8ff1b2de**. Both
  `auto_begin_transaction` and `auto_end_transaction` now route the
  typed `DbError` through `begin_to_resolve_value` /
  `end_to_resolve_value` and resolve via
  `ResolveValue::RejectError(e.to_op_error())`. Two unit tests
  (`auto_begin_transient_error_preserves_code`,
  `auto_end_lock_contention_preserves_code`) pin the contract for the
  two SDK-facing retry codes. Also verified: `grep -n OpResult::Failed
  crates/plugin-db/src/` returns **only doc comments** — every reachable
  path is now `ResolveValue::RejectError`.
- **r2 §6 sweep** — `OpResult::Failed` is gone from `plugin-db` entirely
  (zero reachable sites). The shape can now be removed from the runtime
  `OpResult` enum on plugin-db's behalf.

Regressed since r2:

- **r2 §1 second item** (`migrations.rs:258 tx_connect_failed` strips
  SQLSTATE) — **re-introduced** by `ed697c45`. See §1.1.
- **r2 §7 first item** (`coded_db` double-prefix `"db: {ctx} failed: db:
  ..."`) — **re-introduced** by `ed697c45`. See §7.

Still open from r2 (unchanged):

- **r2 §7 HIGH** — SDK `toJSON` drops `hint` (`sdks/db/src/collection.ts:47-66`).
- **r2 §3** — `DbError::validation_hinted` declared but **unused in
  production** (`grep -n validation_hinted` → only the declaration).
- **r2 §5** — `cic_failed` JS-visible code collides with envelope-internal
  `"code": "validation_refused"`/`"unique_violation"`.
- **r2 §8** — `DbError::Coded` variant declared but unused in production.

Net score delta vs r2: see §10.

---

## 1. `.code` discipline

### [HIGH] `crates/plugin-db/src/migrations.rs:258` — `tx_connect_failed` REGRESSED, discards SQLSTATE again

```rust
let client = backend
    .acquire_dedicated_client()
    .await
    .map_err(|e| coded("tx_connect_failed", &e.into_string(), None))?;
```

**This was fixed in `60ca1ad6` (May 21) and then unfixed in
`ed697c45` (May 22).**

`git show ed697c45 -- crates/plugin-db/src/migrations.rs`:
```
-        .map_err(|e| e.to_op_error())?;
+        .map_err(|e| coded("tx_connect_failed", &e.into_string(), None))?;
```

ed697c45's actual intent was to extract `map_audit_bootstrap_err` from
the four repeated audit-bootstrap closures. That part landed
correctly. But the same diff also rolled back the prior commit's fix
to the line directly below — almost certainly a botched merge / branch
re-base since the commit message does not mention this site at all.

**Why high**: `e.into_string()` strips the `DbError` variant. The most
common acquire-failure mode is `Transient` (pool exhausted, pg
restarting), which the SDK retries when `e.code === "transient"`. With
this code path the SDK sees `.code = "tx_connect_failed"` instead, the
`"transient backend failure; retry after a short backoff"` hint
disappears, and the SDK's default retry policy skips. This is exactly
the bug r2 §1.b called out, now resurrected.

**Fix**: re-apply 60ca1ad6's solution verbatim:

```rust
let client = backend
    .acquire_dedicated_client()
    .await
    .map_err(|e| e.to_op_error())?;
```

`acquire_dedicated_client` already returns `Result<_, DbError>`; the
typed variants flow straight through `to_op_error` with the right
`.code` + `.hint`. No catch-all wrapping required because there is no
operator-context value being added.

**Verification**:
```
git show 60ca1ad6 -- crates/plugin-db/src/migrations.rs
# the fix shipped
git show ed697c45 -- crates/plugin-db/src/migrations.rs | grep -A2 acquire_dedicated_client
# the regression shipped on top
grep -n 'tx_connect_failed' crates/plugin-db/src/
crates/plugin-db/src/migrations.rs:258:        .map_err(|e| coded("tx_connect_failed", &e.into_string(), None))?;
# single site, exactly as before r2
```

---

### [MEDIUM] `crates/plugin-db/src/replication_ops.rs:84,118,155,215` — every `replication::*` helper failure re-codes to `internal`

```rust
match crate::replication::ensure_publication_and_slot(&pool, &app_id).await {
    Ok(out) => OpResult::JsValue { … ResolveValue::String(out.to_json()), … },
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
`Result<_, String>`. Inside that function (line 220-233) the wal_level
misconfig path is explicitly classified and the error string carefully
crafted:

```rust
if msg.contains("55000") || msg.to_lowercase().contains("wal_level") {
    format!(
        "replication: server is not configured for logical \
         decoding — set wal_level=logical in postgresql.conf …"
    )
}
```

…but it's a `String` by the time it reaches the dispatch, where it
gets wrapped as `DbError::Internal`. JS sees `.code = "internal"` for a
fundamentally operator-actionable mis-config. Same for `42710`,
`probe pg_publication`, every other branch.

This is documented in the module preamble (lines 40-46) as a known
gap awaiting a `replication.rs` typed-error sweep — flagged here so
the gap stays on the radar.

**Why medium not high**: not a regression vs r2 (r2 didn't audit
`replication_ops.rs` because the typed-rail commits at the time were
fresh). But the SDK has zero ability to distinguish "wal_level
misconfigured" (operator action) from "transient transient pool
acquisition" (SDK retry) on every `db.replication.*` op. Both arrive
as `.code = "internal"`.

**Fix**: convert `replication.rs` to `Result<_, DbError>` so each
classified failure (wal_level → `Configuration { code:
"replication_wal_level_misconfigured" }`, `42710` →
`Coded { code: "publication_exists" }`, etc.) carries its real
classification. The downstream wrap-in-Internal pattern at four
dispatch sites can then disappear.

**Verification**:
```
grep -n 'Result<.*, String>' crates/plugin-db/src/replication.rs
# 7 hits — every public function and most helpers
grep -n 'DbError::Internal { message: e }' crates/plugin-db/src/replication_ops.rs
# 4 hits — every dispatch wraps the unstructured String
```

---

### [LOW] `crates/plugin-db/src/migrations.rs:317-323` — manual `coded("internal", …)` (carried over from r2 §1c)

Unchanged since r2. The two sibling audit-empty-RETURNING sites build
a `DbError::Internal` via `coded_db`; this one bypasses the pipeline:

```rust
if id == 0 {
    return Err(coded(
        "internal",
        "db: migration insert returned no row",
        None,
    ));
}
```

Net the JS code is the same (`"internal"`); the message shape diverges
from the sibling sites by ~4 characters. Cosmetic. Re-listed here
because nothing changed, not because it ranks high.

---

## 2. Remaining `OpResult::Failed { error: String }` sites

**Zero.** Confirmed by:

```
grep -n 'OpResult::Failed' crates/plugin-db/src/
crates/plugin-db/src/error.rs:247:    /// `Result<_, String>` -> `OpResult::Failed { error: String }` path
crates/plugin-db/src/orchestrator/auto_tx.rs:64:        // legacy `OpResult::Failed { error: String }` rail flattened the
crates/plugin-db/src/orchestrator/auto_tx.rs:298:    //! `OpResult::Failed { error: e.into_string() }`, stripping `.code` /
```

All three hits are doc comments referring to the legacy shape; no
production callers remain. The downstream `OpResult::Failed` enum
variant can now be retired from `crates/runtime/src/core/state.rs` on
plugin-db's behalf (separate review scope; mention so the cleanup
isn't forgotten).

---

## 3. Error message quality — random sample of 8 `Internal` sites

Re-walked fresh, no message-content changes since r2 unless noted.

| # | Message | Source | Rating | Notes |
|---|---|---|---|---|
| 1 | `"replication: pg_create_logical_replication_slot returned no row"` | `replication.rs:243-249` | OK | New post-r2 (commit `c83d6a8c`). Operator-actionable: names the operation. Could include `app_id` + `slot_name`. |
| 2 | `"audit: insert_backfill_running returned no row"` | `audit.rs:617` | POOR | Carried over r2 §2 finding. No `(app_id, collection, name)` triple — operator must grep the audit log to locate the run. |
| 3 | `"audit: INSERT returned no row"` | `audit.rs:329` | POOR | Same. The most generic of the three "no row" guards. |
| 4 | `"db: migration insert returned no row"` | `migrations.rs:320` | POOR | Same. The "where" (which app, which collection) is in the surrounding scope but not the message. |
| 5 | `"db: lazy init failed: {e}"` | `register_model/mod.rs:121` | POOR | Inner `{e}` is `init_pool_async`'s raw error — if the DSN contains a credential fragment, it lands in the JS console reachable by the app code. Flag, not regress. |
| 6 | `"db: apply received a {kind} op outside ChangeClass::Destructive (class={:?}); upstream destructive-class filter (register_model::apply pass1/pass2) should have skipped it. This is a contract violation — refusing to silently no-op."` | `apply.rs:278-285` | GOOD | Already cited in r2; carries the breached invariant, the file and pass, and the operator impact. Verbose for the right reason. |
| 7 | `"db: create index '{name}' exhausted retry budget without a terminal result"` | `backend/postgres.rs:597-600` | GOOD | Names the index, the contract, classified as `cic_configuration`. The CIC orchestrator's loop-exhaustion guard is exactly the bucket that needs a clear "this should never happen" signal. |
| 8 | `"this Migration wrapper has finalised"` (`migration_not_active`) | `migrations.rs:182-191` | GOOD | Carries the recovery hint inline (`"Mint a fresh one via env.db.migrations.start(spec)"`). Note: this is a `coded(...)` call, not an `Internal` — included because it's the model the others should follow. |

### Recurring offenders (unchanged from r2)

1. **"X returned no row"** at 3 sites (`audit.rs:329`, `audit.rs:617`,
   `migrations.rs:320`) — none carry the `(app_id, collection, name)`
   triple that would let an operator pinpoint the failing run from the
   message alone.

2. **Inner-error leakage** (`auto_tx.rs:203`, `transaction.rs:150`,
   `register_model/mod.rs:121`) — `compio_postgres::Error`'s Display
   interpolated unconditionally. DSN/credentials commonly carried.
   Lower-cased version: scrub in `walk_pg_chain` or at the boundary.

3. **Recurring `"db: "` prefix** — see §7.

---

## 4. Hint discipline

### [HIGH] `validation_hinted` declared in `error.rs:287-297` — production callers: still **zero**

```
grep -n 'validation_hinted' crates/plugin-db/src/
crates/plugin-db/src/error.rs:287:    pub fn validation_hinted(
# (no other hits)
```

Carried verbatim from r2 §3. The helper exists; no production call
site uses it. Every coded-but-unhinted error in the table below could
benefit from converting to `validation_hinted(...)`. Pin: zero calls.

### Gap-table re-walk

Sites with no `hint` that should carry one — re-checked after
`8ff1b2de` / `cbd12944` / `c83d6a8c`. New post-cycle sites flagged at
the bottom.

| Code | Hint? | Site | Severity |
|---|---|---|---|
| `migration_cancelled` (mid-run) | NO | `migrations.rs:150-156` | MED — sibling on-start version *has* a thorough hint |
| `migration_not_cancellable` | NO | `migrations.rs:170-176` | MED — user calling cancel on a terminal run has no recovery advice |
| `invalid_argument` (multiple) | NO | `migrations.rs:226-231,361-365,458-460,516-520,525-530,608-616` | LOW |
| `no_active_migration` | NO | `migrations.rs:369-374,377-383,451-455,462-464` | LOW |
| `invalid_collection` | NO | `migrations.rs:218-224` | LOW |
| `migration_already_active` | NO | `migrations.rs:226-231` | LOW |
| `invalid_isolation_level` | NO | `orchestrator/transaction.rs:128-134` | MED — the answer is in the message body, not the hint field |
| `tx_already_active` | NO | `orchestrator/transaction.rs:117-121` | LOW |
| `invalid_app_id` | NO | `audit.rs:823-838` | LOW |
| `lazy_init_failed` | NO | `register_model/mod.rs:119-123` | LOW |
| `backend_not_initialized` | NO | `register_model/mod.rs:125-128` | LOW |
| `not_configured` | NO | `error.rs:269-275` helper, all call sites | MED — cold-start failure, the canonical "what do I do" moment |
| `invalid_filter` / `invalid_collection` / `invalid_identifier` (from `From<QueryError>`) | NO | `error.rs:351-364` | MED — `invalid_filter` benefits from a hint pointing at the operator catalog |
| `tx_connect_failed` | NO | `migrations.rs:258` | HIGH — if §1.1 *isn't* fixed, this site at least needs `Some("transient backend failure; retry after a short backoff")` |
| `audit_bootstrap_failed` (catch-all only) | NO | `migrations.rs:119-123` | LOW |
| `cic_failed` (envelope already carries `reason`) | NO | `backend/postgres.rs:413-424` | LOW — the JSON body is the hint analog |
| `cic_configuration` (loop-exhaustion) | NO | `backend/postgres.rs:595-600` | LOW |
| `not_provisioned` (post-r2 add) | NO | `replication_ops.rs:230-234` | LOW |

### Post-cycle additions vs r2

The new code in `lock_guard.rs` does NOT add coded JS-visible errors —
`acquire()` returns the underlying `DbError` from
`backend.acquire_advisory_lock(...)` verbatim, and the catastrophic
drop path only logs (`tracing::error!`). No new hint gap; the design
defers to whatever `acquire_advisory_lock` itself returns. Good.

`auto_tx.rs`'s new conversion helpers (`begin_to_resolve_value`,
`end_to_resolve_value`) route via `e.to_op_error()`, so the SQLSTATE
variants pick up their canonical hints from `error.rs:221-235`
(serialization/lock/transient all carry hints). This is exactly the
right pattern — no new gap.

`replication.rs:243-249`'s new `DbError::Internal` carries no hint,
but `Internal` is the catch-all and the operator-actionable cases
should be re-classified upstream (see §1.2) rather than hint-padded.

---

## 5. Retry semantics

### [MEDIUM] auto-tx COMMIT path is fixed; verify the SDK actually retries

The HIGH-impact gap in r2 (auto-tx COMMIT-time `Transient` reaching JS
as a flat string) is closed: `auto_end_transaction` ⇒
`end_to_resolve_value(Err(DbError::Transient { … }))` ⇒
`ResolveValue::RejectError(OpError::coded("transient", msg,
Some("transient backend failure …")))`. The test
`auto_end_lock_contention_preserves_code` pins one of the two retryable
codes the path emits.

**Pin coverage gap**: the test covers `LockContention`; the
companion test covers `Transient` on the *begin* path. Neither
covers `Transient` or `Serialization` on the *end* (COMMIT) path,
which is precisely the case r2 flagged as the highest-impact retry
moment. The actual conversion helper is shared (`end_to_resolve_value`
takes any `Result<(), DbError>`) so the code path is exercised; only
the test naming claims "lock contention" not "serialization at
commit". Low risk.

**Fix**: add one more test, `auto_end_serialization_at_commit_preserves_code`,
constructing `Err(DbError::Serialization { … })` and asserting `.code
== "serialization_failure"`. ~10 LOC.

### [LOW] No `retryable: bool` wire flag (unchanged from r2 §4)

The SDK still discriminates retryable errors by `.code ∈ {"transient",
"serialization_failure", "lock_not_available"}`. Adding new retryable
codes later (e.g. `optimistic_lock_failure`) requires SDK predicate
updates. A boolean `retryable: true` field stamped onto the
`OpError::coded` for the three known codes would let the SDK write
`if (e.retryable)` and never miss future additions. Same reasoning as
r2; functional impact is small today.

---

## 6. `SchemaRefused` payload shape

### [MEDIUM] `cic_failed` envelope `code` collision with `e.code` (unchanged from r2 §5)

The envelope shape at `backend/postgres.rs:413-424` is unchanged. The
outer `DbError::SchemaRefused { code: "cic_failed", … }` stamps
`e.code = "cic_failed"` on the JS exception, but the JSON envelope
inside `e.message` carries its own `"code"` field whose value is one of:

- `"validation_refused"` (lines 491-500 — index-landed-INVALID,
  573-585 — non-transient build failure)
- `"unique_violation"` (lines 532-540 — data-violation path)

Two `"code"` fields with two meanings in one exception. SDK callers
expecting `e.code === "validation_refused"` will miss the
CIC-validation path.

Carried forward verbatim. Fix is unchanged: rename the
envelope-internal field to `"reason"`, OR align `e.code` to the
envelope-internal code and move the CIC origin marker to `"source":
"cic"`. The latter better matches the documented contract ("branch on
`e.code`, parse `e.message` for details").

### [LOW] Envelope has no machine-typed `destructive_pending` schema

Same as r2 §5 second half. The SDK's `mapNativeError` returns the
error unchanged; consumers must `JSON.parse(e.message)` and walk
`destructive_pending` themselves. A typed `ValidationRefusedError`
subclass in `@zeroship/db` would let `mapNativeError` materialise the
violations / destructive list eagerly.

---

## 7. Mixed error shapes — JS-side consistency

### [HIGH] `coded_db` re-introduced the `"db: {ctx} failed: db: ..."` double-prefix (REGRESSION)

`crates/plugin-db/src/migrations.rs:82-98`:

```rust
fn coded_db(context: &str, e: crate::error::DbError) -> OpError {
    let mut db_err = e;
    match &mut db_err {
        … | DbError::Internal { message } => {
            *message = format!("db: {context} failed: {message}");
        }
        _ => {}
    }
    db_err.to_op_error()
}
```

The same `ed697c45` that re-introduced the `tx_connect_failed` regression
ALSO reverted `coded_db`'s prefix from `"{context}: {message}"`
(60ca1ad6) back to `"db: {context} failed: {message}"`. Combined with
`walk_pg_chain`'s own `"db: "` prefix, every migrations-stage SQL
error now reads:

```
db: migration insert failed: db: <pg-message>
```

The operator log noise doubles. r2 §7 (originally inherited from r1
§4e) flagged exactly this; the May 21 commit fixed it; the May 22
commit unfixed it.

**Fix**: revert `coded_db`'s format string to the May 21 form:
```rust
*message = format!("{context}: {message}");
```

Both prefixes are needed by *neither* the SDK (which branches on
`.code`) nor the operator (who reads the `.code` first, then the
message body). Keep one or the other; never both.

**Verification**:
```
git show 60ca1ad6 -- crates/plugin-db/src/migrations.rs | head -40
# fix shipped
git show ed697c45 -- crates/plugin-db/src/migrations.rs | head -10
# revert shipped
sed -n '82,98p' crates/plugin-db/src/migrations.rs
# current state: doubled
```

### [HIGH] SDK `toJSON` STILL drops `hint` (unchanged from r2 §7)

`sdks/db/src/collection.ts:47-66`:

```ts
const obj: Record<string, unknown> = {
    name: (this as Error).name,
    message: (this as Error).message,
};
const code = (this as { code?: unknown }).code;
if (code !== undefined) obj.code = code;
const errs = (this as { errors?: unknown }).errors;
if (errs !== undefined) obj.errors = errs;
return obj;
```

Still no `hint` propagation. Every retryable code's actionable
recovery advice (now correctly stamped at native via `8ff1b2de`)
silently disappears at the RPC boundary. This is the 3-LOC fix r2
ranked #1 by SDK-author impact; it is still **not landed**.

The cross-crate caveat applies — this lives in `sdks/db`, not
`crates/plugin-db`. But it remains the single biggest hint-discipline
gap in the whole error rail.

**Fix**: 3 lines added to `sdks/db/src/collection.ts`:
```ts
const hint = (this as { hint?: unknown }).hint;
if (hint !== undefined) obj.hint = hint;
```

### [MEDIUM] `mapNativeError` short-circuits on `.code` but never inspects `.hint` (unchanged from r2 §7)

`sdks/db/src/errors.ts:67-69` — pass-through when `.code` is a string,
but never copies `e.hint` into the structured Error shape that crosses
the RPC boundary. Same root cause as the `toJSON` gap.

### [MEDIUM] Inconsistent error-source prefix across `migrations` / `audit` / `register_model`

Re-listed because §7.1 reverted a sub-finding. Net state today:

- `audit.rs` → `"audit: <ctx>: <pg-message>"`
- `migrations.rs::coded_db` → `"db: <ctx> failed: <pg-message>"` (regressed)
- `migrations.rs::coded("audit_bootstrap_failed", "audit bootstrap failed: <body>")` — different shape again (via `map_audit_bootstrap_err`)
- `walk_pg_chain` prepends `"db: "` to every Postgres error
- Compound shapes today: `"audit: create_idx: db: <real msg>"` and `"db: migration insert failed: db: <real msg>"`

For the SDK consumer the impact is minor (they branch on `.code`).
For the operator reading worker logs, the inconsistency is noise.

**Fix**: remove the `"db: "` prefix from `walk_pg_chain` since every
downstream wrapper already adds context; OR drop both `"audit: "` and
`"db: "` prose prefixes entirely since `.code` already encodes the
subsystem (`unique_violation`, `audit_bootstrap_failed`, …).

---

## 8. The `DbError::Coded` variant remains production-unused (unchanged from r2 §8)

```
grep -n 'DbError::Coded' crates/plugin-db/src/
crates/plugin-db/src/error.rs:40://! | [`DbError::Coded`] | … |
crates/plugin-db/src/error.rs:239:            DbError::Coded { code, message, hint } => OpError::coded(code, message, hint),
crates/plugin-db/src/error.rs:264:            | DbError::Coded { message, .. }
crates/plugin-db/src/error.rs:327:            | DbError::Coded { message, .. }
crates/plugin-db/src/error.rs:449:        let e = DbError::Coded {
```

Only the variant declaration, the impl arms, and one test. Every
migrations lifecycle code (`migration_already_running`,
`migration_cancelled`, etc.) still builds an `OpError` directly via
`coded(...)`, bypassing the `DbError::Coded` envelope. The doc-comment
promises a future where the helpers flow `DbError` through; that
future has not landed.

Neutral, not a regression. Either start using it (route
`migrations::coded()` through `DbError::Coded → to_op_error`) or
delete the variant and the unused passthrough test.

---

## 9. Concrete fixes ranked by SDK-author impact (deltas vs r2)

| Rank | Finding | LOC est. | Files | Status vs r2 |
|---|---|---|---|---|
| 1 | §7 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts` | UNCHANGED |
| 2 | §1.1 — REGRESSION: `tx_connect_failed` strips SQLSTATE | 2 | `migrations.rs:258` | REGRESSED — revert ed697c45's change at that line, restore 60ca1ad6's form |
| 3 | §7 — REGRESSION: `coded_db` doubled `"db: "` prefix | 1 | `migrations.rs:93` | REGRESSED — same commit, same revert |
| 4 | §1.2 — `replication.rs` returns `Result<_, String>`; all 4 dispatch sites re-code as `internal` | ~30 | `replication.rs`, `replication_ops.rs` | NEW — surfaced this round because r2 hadn't audited replication |
| 5 | §4 — add hints to `migration_cancelled` (mid-run), `migration_not_cancellable`, `invalid_isolation_level`, `not_configured`, `From<QueryError>::invalid_filter` | ~20 | `migrations.rs`, `transaction.rs`, `error.rs` | UNCHANGED |
| 6 | §6 — `cic_failed` envelope `code` collides with `e.code` | ~10 | `backend/postgres.rs:413-424` + tests | UNCHANGED |
| 7 | §5 — add `auto_end_serialization_at_commit_preserves_code` test | ~10 | `auto_tx.rs` tests | NEW — the conversion path is shared but `Serialization` at COMMIT isn't directly pinned |
| 8 | §3 — augment "INSERT returned no row" messages with `(app_id, collection, name)` triple | ~10 | `audit.rs:329,617`, `migrations.rs:320` | UNCHANGED |
| 9 | §4 — actually use `DbError::validation_hinted` (production count: 0) | ~15 sites | scattered | UNCHANGED |

---

## 10. Score

**71 / 100** (+4 vs r2's 67)

R2's headline HIGH (auto_tx flattening `DbError`) is genuinely closed
by `8ff1b2de` — well-named conversion helpers, tests pinning the
contract, the canonical `OpResult::JsValue + ResolveValue::RejectError`
pattern. That's a real +9 on the rubric.

But `ed697c45` *also* re-introduced TWO sub-findings r2 listed as
resolved at HEAD: the `tx_connect_failed` SQLSTATE strip and the
`coded_db` doubled-prefix. That's a real -5. Net +4.

The +9/-5 numerics aren't arbitrary: the close was a major HIGH (was
the single biggest gap); the regressions are a small HIGH plus a
MEDIUM, both 1-3 LOC reverts. So the *direction* of progress is good,
but the *care* this round was lower than the prior — a single commit
silently reverted two sibling fixes while extracting an unrelated
helper.

What's strong:

- The `DbError` taxonomy + `to_op_error()` continues to be a clean,
  single-source-of-truth design.
- Test coverage at the conversion boundary is excellent — the auto_tx
  tests are model unit tests for "code preservation at the dispatch
  rail".
- The `lock_guard.rs` extraction is a textbook example of replacing
  three open-coded sites with one typed invariant. Error path
  unchanged (delegates to `acquire_advisory_lock`'s already-typed
  `DbError`).
- The `c83d6a8c` empty-RETURNING fix mirrors the prior `audit.rs`
  shape — discipline matched.

What's weak:

- Cherry-picks / merges silently reverting hand-fixed lines is a
  process problem the diff review should have caught. Both regressions
  in `ed697c45` are visible in the diff hunk *of that commit*; neither
  is in the commit message; neither is covered by a unit test (the
  `map_audit_bootstrap_err` extraction the commit advertises *is*
  test-covered, but the two reverts surrounding it are not).
- The 3-LOC `hint` fix in `sdks/db` continues to be the highest-impact
  unaddressed item and continues to slip. Every other hint
  improvement in the native layer is invisible past the RPC boundary
  until this lands.
- `replication.rs::*` and the SDK helpers ride entirely on
  `Result<_, String>`. The dispatch sites obediently wrap them as
  `Internal`, losing the operator-actionable distinction between
  wal_level misconfiguration (operator action) and pool exhaustion
  (SDK retry). Refining this is straightforward; nothing in the rail
  has yet been allocated time to it.

If the two regressions are reverted (~3 LOC), if §7 (SDK `toJSON`)
lands (~3 LOC), and if `replication.rs` graduates to `Result<_,
DbError>` (~30 LOC including the 4 dispatch unwraps), this would land
at ~85/100. The remaining 15 are reserved for: production usage of
`validation_hinted` and `DbError::Coded` (~zero today), the
`SchemaRefused` envelope schema shape, the `retryable: bool` wire flag,
and the documented public code catalog
(`docs/reference/db-error-codes.md`) still missing.
