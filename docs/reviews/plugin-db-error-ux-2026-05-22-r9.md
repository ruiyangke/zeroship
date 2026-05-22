# plugin-db Error-UX Review — 2026-05-22 r9

Scope: `crates/plugin-db/src/` at HEAD (post `389749ca`, `7d0bc4c5`,
`757026e3`, `bed655c1`, `7bd2187e`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r8.md` (91 / 100).

Lens: SDK-author error-handling discipline. Every finding evaluates
the JS-visible surface — `e.code`, `e.message`, `e.hint` — and whether
the SDK can branch on it without parsing strings. r8 flagged two new
MEDs (cold-init code drift; `not_configured` 4-way overload); r9
verifies whether `389749ca` + `7d0bc4c5` closed them and re-walks the
landscape fresh.

---

## TL;DR — what landed since r8

**Resolved:**

- **r8 §3 / MED-NEW — cold-init code drift between
  `lazy_init_failed` and `not_configured` for the same
  `init_pool_async` failure** (commit `7d0bc4c5`). The two `exec.rs`
  sites at `:64` and `:317` previously stamped `not_configured`; they
  now both stamp `lazy_init_failed`, matching the third call site at
  `orchestrator/register_model/mod.rs:120`. Grep confirms 3
  production literal sites for `"lazy_init_failed"` all pointing at
  the same underlying call (`crate::init_pool_async()`). The SDK now
  sees one canonical code for cold-init failures. **Closes r8 §3
  primary drift.**
- **r8 §3 / second MED-NEW — `not_configured` overloaded backend-not-
  initialised in 2 of the 4 conditions** (commit `389749ca`). The
  `v8_classes/migration.rs:269` and `v8_classes/migrations.rs:180`
  sites previously stamped `not_configured` for the
  `context.backend() == None` invariant; they now stamp
  `backend_not_initialized`, matching the canonical site at
  `orchestrator/register_model/mod.rs:127`. The UK→US spelling drift
  in the message body (r8 §6 LOW-NEW: "initialised" vs
  "initialized") was also unified to US spelling. **Closes r8 §3
  case 4 + r8 §6 spelling drift.**

**Carry from r8 (still open):**

- r8 §3 / MED — `not_configured` still overloads ≥2 distinct
  conditions (see §3 below — narrower but not resolved).
- r8 §2.LOW — 5 P0001 `session_*` codes still `validation()` callers,
  zero `validation_hinted()` callers. Fourth cycle in carry.
- r8 §2 LOW-NEW — `lazy_init_failed` + `cic_configuration` still
  hint-less; the natural pair-up with the §3 unification did not
  happen (the unification commit `7d0bc4c5` did NOT add a hint).
- r8 §5 #5 — `audit.rs:818` `invalid_app_id` message still doesn't
  name the `[A-Za-z0-9_-]` alphabet.
- r8 §6 — `finalise_backfill` warn shape still missing `name` +
  `collection` fields (both are in lexical scope at the call site).
- r8 §7 — `prefix_message` wildcard `_ => {}` arm bypasses
  `#[non_exhaustive]` future variants.
- **`config_hinted()` and `validation_hinted()` BOTH have zero
  production callers** — see §4 (sharper than r8's reading).
- Cross-crate SDK gaps: `toJSON` drops `hint`; `withRetry` default
  predicate only matches `optimistic_lock_failure`.

**New findings in r9:**

- **[INFO/LOW] §4 — `config_hinted()` has ZERO production callers,
  not 2 as r8 reported.** Re-verifying with grep, the two
  hint-bearing Configuration sites (`wal_consumer.rs:349-358`,
  `replication.rs:277-286`) both construct `DbError::Configuration {
  code, message, hint }` as a struct literal, not via the helper.
  The helper exists but no production site uses it. r8 §1 PASS table
  ("`config_hinted()` helper at `error.rs:311-321` exists for the
  hint-bearing path") was strictly true; r8 §4 INFO ("`f1c5184e`
  shipped `config_hinted()` AND landed 2 production callers in the
  same commit") was inaccurate — the commit shipped the helper +
  the two hint-bearing sites, but the sites do not call the helper.
  Treat the discipline-split as: 2 of 6 Configuration sites carry
  hints (33%, unchanged); 0 of 6 use the dedicated constructor
  (0%). Architecture-r9 §M15 captured this independently.
- **[INFO] §3 — the remaining `not_configured` overload is narrower
  but still ≥2 conditions across 4 sites**, not resolved.

**Net score delta vs r8:** see §10.

---

## 1. Verify r8 closures end-to-end

### [PASS] r8 §3 primary MED — `lazy_init_failed` unification

Verified at:

- `exec.rs:64` — `init_pool_async()` lazy-init in `run_sql`:
  `.map_err(|e| DbError::config("lazy_init_failed", format!("db: lazy init failed: {e}")))?;`
- `exec.rs:317` — `init_pool_async()` in `ensure_pool`: identical
  shape.
- `orchestrator/register_model/mod.rs:117-123` — same
  `init_pool_async()` call, `code: "lazy_init_failed"`.

All three call the same primitive; all three stamp the same
`.code`. Pinned in the `error.rs` preamble at lines 30-34:

```
//! 4. **Cold-init**: `lib.rs::init_pool_async` returns `Result<_, String>`;
//!    the call sites at `orchestrator/register_model/mod.rs:120` and
//!    `exec.rs::ensure_pool` synthesise `DbError::Configuration` with
//!    code `lazy_init_failed` (unified at `7d0bc4c5`; same code at
//!    every cold-init call site).
```

Honest documentation: the prior cycle's documented drift is now
documented as closed.

### [PASS] r8 §3 case 4 + r8 §6 spelling — `backend_not_initialized` unification

Grep returns 3 production literal sites for `"backend_not_initialized"`:

- `orchestrator/register_model/mod.rs:127` — `code:
  "backend_not_initialized"`, message `"db: backend not initialized"`.
- `v8_classes/migrations.rs:180` — `"backend_not_initialized"`,
  message `"db: backend not initialized"` (was `not_configured` +
  "initialised").
- `v8_classes/migration.rs:269` — `"backend_not_initialized"`,
  message `"db: backend not initialized"` (was `not_configured` +
  "initialised").

Three sites, one code, one spelling. The §6 LOW-NEW spelling drift
is gone — the codebase now uses US spelling uniformly for this
condition. **PASS.**

### [PASS] All r5/r7 closures still clean

- Zero hits for `ConsumerError::NotProvisioned`.
- Zero hits for `msg.contains("P0001")` patterns.
- `auth/session.rs::classify_p0001_detail` uses
  `db_err.code() != &SqlState::RAISE_EXCEPTION` and
  `classify_detail_token` map (lines 174-215). No substring
  match on SQLSTATE codes.
- `replication.rs:216-221` + `:268-274` use
  `e.as_db_error()?.code() == &SqlState::<NAME>` typed checks.

The "discriminate on machine ids, not formatted strings" rule remains
applied uniformly across the rail.

---

## 2. Verify r8 closure end-to-end — preamble accuracy

`error.rs:9-42` was rewritten in `9e392ba1` (r8) to enumerate 5
categories of `Result<_, String>` hold-outs. Cycle 09:47 (commit
`757026e3`, "docs hold-outs") tightened the preamble at category 4
to reflect the closure:

> **Cold-init**: `lib.rs::init_pool_async` returns `Result<_, String>`;
> the call sites at `orchestrator/register_model/mod.rs:120` and
> `exec.rs::ensure_pool` synthesise `DbError::Configuration` with
> code `lazy_init_failed` (unified at `7d0bc4c5`; same code at
> every cold-init call site).

The preamble matches the code. No stale prose to refute the cycle's
audit. **PASS.**

---

## 3. `not_configured` remaining overload — narrower but still ≥2 conditions

### [LOW-CARRY / promoted to MED-CARRY] §3 not resolved

Grep returns 5 production literal sites for `"not_configured"`
(excluding the `bootstrap.rs` test enumerator and the `error.rs`
unit-test fixture):

| Site | Underlying condition |
|---|---|
| `exec.rs:68` | Pool slot empty AFTER `init_pool_async()` claimed success (invariant breach) |
| `exec.rs:321` | Same as `:68` (pool slot empty in `ensure_pool`) |
| `orchestrator/transaction.rs:147` | `db_url` not set at all in per-isolate context |
| `orchestrator/auto_tx.rs:199` | Same as `transaction.rs:147` |

The r8 cardinality was "4 conditions across 5/6 sites". Post-r8
unification:
- Cold-init failure → `lazy_init_failed` (out of `not_configured`).
- Backend trait not minted → `backend_not_initialized` (out of
  `not_configured`).

**Two conditions remain bundled under `not_configured`:**

1. **Pool slot empty after init claimed success** (invariant breach;
   ought to be `pool_not_initialized` or similar; `debug_assert`-
   equivalent — should never happen in production).
2. **`db_url` literally unset in per-isolate context** (operator
   config error; should be `db_url_unset` or the message should
   carry the canonical remediation: "set DB_URL on the runtime
   context").

An SDK author cannot tell these apart from `.code` alone — the
former is "file a bug, this should never happen", the latter is
"set your env var". Different remediations, same code.

That said: **the r8 finding was MED on a 4-way overload spanning
remediable and invariant conditions; the r9 residue is a 2-way
overload, both of which are operator-side** (one is invariant-class,
one is config-class, but neither is user-input and neither is
remediable by SDK retry). The MED is closer to LOW-MED now.

**Fix sketch (~6 LOC)**: pick one new code (e.g.
`pool_not_initialised` or `db_url_unset`) and disambiguate the
invariant-breach sites (`exec.rs:68`, `:321`) from the config sites
(`transaction.rs:147`, `auto_tx.rs:199`). Or simply add a hint to
each so the SDK can surface the right action even with the same
`.code`. Either closes the gap.

Severity: **LOW-MED** — narrower than r8's MED-NEW; same fragility
class but smaller blast radius.

### [INFO] Cross-class summary — every `init_pool_async` cold-init now stamps `lazy_init_failed`

The r8 MED was "same call → 2 codes". `7d0bc4c5` makes that "same
call → 1 code, with consistent message body". Per the r8 §3 fix
sketch — `cold_init_failed` would have been a cleaner name than
`lazy_init_failed`, but the unification picked the existing name to
avoid breaking SDK consumers who may already branch on the original
`register_model` code. Defensible call.

---

## 4. `config_hinted` + `validation_hinted` usage — re-verify with fresh grep

### [INFO/LOW-NEW] BOTH constructors have zero production callers

```
grep -rn 'validation_hinted\|config_hinted' crates/plugin-db/src/
# 3 hits total — all in error.rs:
#   error.rs:299 (doc comment pointer)
#   error.rs:311 (config_hinted declaration)
#   error.rs:333 (validation_hinted declaration)
```

Re-checked the two hint-bearing Configuration sites:

`wal_consumer.rs:349-358`:
```rust
return Err(DbError::Configuration {
    code: "not_provisioned",
    message: "wal consumer: db_url not configured".to_string(),
    hint: Some(
        "replication requires a connected runtime context — set \
         DATABASE_URL or pass --db-url so the runtime can mint a \
         replication=database connection"
            .to_string(),
    ),
});
```

`replication.rs:277-286`:
```rust
DbError::Configuration {
    code: "wal_level_not_logical",
    message: format!(
        "replication: server is not configured for logical decoding (underlying: {msg})"
    ),
    hint: Some(
        "set wal_level=logical in postgresql.conf and restart"
            .to_string(),
    ),
}
```

Both are struct literals. The dedicated `DbError::config_hinted(code,
message, hint)` constructor at `error.rs:311-321` exists with zero
production calls. Same shape as `validation_hinted()` at lines
333-343 — declared, never called.

**This sharpens r8 §4 (zero `validation_hinted` callers) — it's
actually a symmetric finding: 0 of 6 Configuration sites + 0 of all
ValidationFailed sites use the dedicated constructors.** The
constructors exist for self-documentation; their absence at call
sites is purely a hygiene gap. The wire shape is identical between
struct literal and constructor call — no SDK impact, just rail
internal consistency.

Severity: **LOW** (cosmetic / hygiene) — the constructors aren't load-
bearing for the wire shape. But it does undercut the rail's
self-documentation claim ("the hint-discipline split is now visibly
lopsided: 2 of 6 Configuration sites carry hints; 0 of 5 session-
validation sites do" from r8 — still true, but the 2 that DO carry
hints don't use the dedicated constructor either).

**Fix sketch (~12 LOC)**: switch the 2 hint-bearing Configuration
struct literals to `DbError::config_hinted(code, message, hint)` so
the constructor has live callers (proves it's not dead code) and
then adopt `validation_hinted` for the 5 `auth/session.rs` codes per
r7/r8 carry. Or DELETE the two helpers entirely — the codebase has
demonstrably moved past needing them. Pick one direction.

---

## 5. `finalise_backfill` warn shape — re-verify the r8 §6 carry

At `migrations.rs:639-647`:

```rust
tracing::warn!(
    app_id = %app_id,
    audit_id = audit_id,
    terminal = ?terminal,
    error = %e,
    "finalise_backfill failed; audit row may stay in 'running' \
     status until next reset() — investigate if the operator \
     sees stuck migrations"
);
```

Available in lexical scope at this call site (verified by reading
the function signature at `exec_commit_batch` line 439 + the
`lock_snapshot()` destructuring at line 450):

```rust
let Some((name, collection, audit_id, dry_run, start_generation)) = lock_snapshot() else {
    ...
};
```

`name` and `collection` are local variables at this point. Adding
them to the warn fields is a 2-LOC delta:

```rust
tracing::warn!(
    app_id = %app_id,
    audit_id = audit_id,
    name = %name,             // ← added
    collection = %collection, // ← added
    terminal = ?terminal,
    error = %e,
    "finalise_backfill failed; ..."
);
```

Severity: **LOW** (observability gap, not a wire-shape change) —
unchanged from r8 §6 / r7 §5 / r6 §5. **5th cycle in carry.**

---

## 6. Sample 5 error sites — clarity audit

Walking 5 fresh sites end-to-end (different from r8's sample of 8).

### #1 — `wal_consumer.rs:349-358` — `not_provisioned` Configuration

```rust
DbError::Configuration {
    code: "not_provisioned",
    message: "wal consumer: db_url not configured".to_string(),
    hint: Some(
        "replication requires a connected runtime context — set \
         DATABASE_URL or pass --db-url so the runtime can mint a \
         replication=database connection"
            .to_string(),
    ),
}
```

- `.code`: stable static. ✓
- Message: names subsystem (`wal consumer:`) + condition (`db_url
  not configured`). ✓
- Hint: names two concrete remediations (`DATABASE_URL` env var,
  `--db-url` CLI flag) AND the required connection mode
  (`replication=database`). Genuine remediation, not paraphrase.
- **Verdict**: exemplar. The hint pattern is the rail's gold
  standard.

### #2 — `replication.rs:84-88` — `invalid_app_id` ValidationFailed (empty)

```rust
return Err(DbError::validation(
    "invalid_app_id",
    "replication: app_id must not be empty",
));
```

- `.code`: stable static. ✓
- Message: subsystem + condition. ✓
- No hint, but the message IS the remediation ("must not be empty"
  = "pass a non-empty app_id"). ✓
- **Verdict**: clear. No improvement needed.

### #3 — `replication.rs:91-97` — `invalid_app_id` (bad char)

```rust
return Err(DbError::validation(
    "invalid_app_id",
    format!(
        "replication: app_id contains invalid character {c:?} \
         — only [A-Za-z0-9_] permitted"
    ),
));
```

- `.code`: stable static. ✓
- Message: names the offending character AND the valid alphabet.
- **Verdict**: exemplar for this class. The `audit.rs:816-819`
  twin (next site) should adopt this shape.

### #4 — `audit.rs:816-819` — `invalid_app_id` (bad char) — STILL BELOW STANDARD

```rust
return Err(DbError::validation(
    "invalid_app_id",
    format!("audit: invalid app_id: {name}"),
));
```

- `.code`: stable static. ✓
- Message: names the value, but NOT the valid alphabet. A developer
  who hits this needs to grep the source to learn that `[A-Za-z0-9_-]`
  is the rule (the `validate_app_id` body at lines 812-815).
- **Verdict**: below the `replication.rs:91-97` standard. Same gap
  r8 §5 #5 flagged. 1-LOC fix: append `" — only [A-Za-z0-9_-]
  permitted"` like the replication twin does. Note the alphabet
  differs (`-` is permitted by audit, not by replication). **Carry.**

### #5 — `orchestrator/transaction.rs:118-121` — `tx_already_active` (nested)

```rust
return Err(DbError::validation(
    "tx_already_active",
    "db: transaction already active (nested transactions not supported)",
));
```

- `.code`: stable static. ✓
- Message: states the condition AND that the API doesn't allow it
  (so the developer knows the fix is "wait for the outer tx to
  settle", not "this is a transient retry").
- **Verdict**: clear. Could carry a hint pointing at the
  `db.transaction()` documentation URL, but the message is
  self-sufficient.

**Sample observations:**

- 4 of 5 sites are uniformly clear (subsystem prefix + condition,
  stable code, message-as-remediation when no hint).
- 1 of 5 (audit's `invalid_app_id` bad-char branch) is the
  carry-over from r8 — message inferior to its `replication.rs`
  twin.
- Site #1 demonstrates the gold-standard hint pattern (the only one
  in this sample, and one of 2 total in the codebase).

---

## 7. Retry semantics — re-verify alignment (unchanged from r8)

| `.code` | Variant | Auto-retry? | Hint? | r9 verdict |
|---|---|---|---|---|
| `transient` | `Transient` | YES | YES | ✓ unchanged |
| `serialization_failure` | `Serialization` | YES | YES | ✓ unchanged |
| `lock_not_available` | `LockContention` | YES | YES | ✓ unchanged |
| `unique_violation` | `UniqueViolation` | NO | NO | ✓ unchanged |
| `fk_violation` | `FkViolation` | NO | NO | ✓ unchanged |
| `not_null_violation` | `NotNullViolation` | NO | NO | ✓ unchanged |
| `check_violation` | `CheckViolation` | NO | NO | ✓ unchanged |
| `validation_refused` | `SchemaRefused` | NO | NO (envelope IS the message) | ✓ correct |
| `not_provisioned` | `Configuration` | NO | YES | ✓ unchanged |
| `wal_level_not_logical` | `Configuration` | NO | YES | ✓ unchanged |
| `lazy_init_failed` | `Configuration` | NO | NO | ✓ (post-§3 unification) |
| `cic_configuration` | `Configuration` | NO | NO | ✓ (carry — see §2 LOW from r8) |
| `not_configured` | `Configuration` | NO | NO | ⚠ overloads 2 conditions (see §3) |
| `backend_not_initialized` | `Configuration` | NO | NO | ✓ unified at `389749ca` |

Retryability remains correctly aligned. The post-`f1c5184e` +
`7d0bc4c5` + `389749ca` state has the Configuration class fully
consistent on the `.code → underlying-call` axis except for the
residual `not_configured` overload from §3.

Pinned by `retryable_variants_carry_hint` test
(`error.rs:646-662`) — all 3 retryable variants carry hints; none
of the 6 non-retryable ones do. The test catches a regression where
a Configuration without a hint slot accidentally became retryable.

### [PASS] Cross-crate `withRetry` predicate (unchanged from r8)

Re-verified at `sdks/db/src/with-retry.ts:25-30` — default predicate
matches `optimistic_lock_failure` only. None of the native rail's 3
retryable codes (`transient`, `serialization_failure`,
`lock_not_available`) picked up by the default. Same carry as r6/r7/
r8.

### [LOW-CARRY] `toJSON` drops `hint` (unchanged from r8)

Re-verified at `sdks/db/src/collection.ts:46-65`. `toJSON` picks up
`name`, `message`, `code`, `errors`. Does NOT pick up `hint`. The
two hint-bearing Configuration codes (`not_provisioned`,
`wal_level_not_logical`) and the three hard-coded retryable hints
all drop their hint when the SDK error is RPC-serialised.

3-LOC fix in SDK. Cross-crate, tracked. Same carry.

---

## 8. Other observations

### [INFO] `prefix_message` wildcard arm — unchanged (r6 §6, r7 §7, r8 §7)

`error.rs:386` still uses `_ => {}` for structured-variant arms.
`DbError` is `#[non_exhaustive]` — a future SQLSTATE-derived
variant added without thinking about prefix semantics would
silently bypass. Tripwire-only; pinned by
`prefix_message_leaves_structured_variants_alone` test at
`error.rs:786-848` for the 4 currently-skipped variants.

### [PASS] `auth/bootstrap.rs:1083-1090` test pins which codes never get prefixed

```rust
"wal_level_not_logical",
"not_configured",
"session_signature_expired",
"session_nonce_replay",
"session_invalid_signature",
"session_invalid_actor_kind",
"session_nonce_too_short",
```

In-tree pin for the rail invariant — structured variants resist
prefix. Solid defensive coverage; the post-`389749ca` codebase
should add `"backend_not_initialized"` to this enumerator next
cycle to keep the pin honest.

### [INFO / hygiene observation] r8 §4 INFO was inaccurate

The r8 §4 INFO claim that `f1c5184e` "shipped `config_hinted()` AND
landed 2 production callers in the same commit" was wrong on the
second clause. `f1c5184e` shipped the helper AND the two
hint-bearing struct-literal sites, but the sites do not call the
helper. r9 §4 sharpens the actual state: 0 of 6 Configuration sites
use the dedicated constructor; 0 of all ValidationFailed sites use
`validation_hinted()`. Either adopt the constructors or delete them.

---

## 9. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC | Files | Status vs r8 |
|---|---|---|---|---|
| 1 | r7 §10#1 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts:46-65` | UNCHANGED (4th carry) |
| 2 | r7 §2.LOW — adopt `validation_hinted()` for 5 `session_*` codes | ~25 | `auth/session.rs:259` (classify_detail_token mapper) | UNCHANGED (4th carry) |
| 3 | **§3 NEW — disambiguate residual `not_configured` overload (2 conditions, 4 sites)** | ~6 | `exec.rs:68,:321`, `transaction.rs:147`, `auto_tx.rs:199` | DOWNGRADED (was MED, now LOW-MED) |
| 4 | r8 §2 LOW — add hints to `lazy_init_failed` + `cic_configuration` (4 sites) | ~6 | `exec.rs:64,:317`, `register_model/mod.rs:119`, `backend/postgres.rs:593-600` | UNCHANGED |
| 5 | r7 §10#4 — SDK `withRetry` predicate add native rail's 3 retryable codes | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 6 | r7 §5 — `finalise_backfill` warn: add `name` + `collection` | 2 | `migrations.rs:639-647` | UNCHANGED (5th carry) |
| 7 | **§4 NEW — adopt `config_hinted()` at 2 struct-literal sites, OR delete both unused constructors** | ~12 (adopt) / ~30 (delete) | `wal_consumer.rs:349-358`, `replication.rs:277-286`, `error.rs:311-343` | NEW (INFO/LOW) |
| 8 | r8 §5 #5 — name alphabet inline in `audit.rs:818` `invalid_app_id` | 1 | `audit.rs:818` | UNCHANGED (2nd carry) |
| 9 | r6 §6 — replace `_ => {}` in `prefix_message` with explicit structured arms | ~10 | `error.rs:386` | UNCHANGED |
| 10 | r6 §10#8 — augment `migrations.rs:329` "returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs` | UNCHANGED |
| 11 | r6 §10#10 — stamp `retryable: true` wire flag on `OpError::coded` | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 12 | r6 §10#11 — route `migrations::coded()` through `DbError::Coded` or delete | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

Rank 4 is the natural pair for rank 3: if §3 disambiguation lands by
adding hints (rather than splitting codes), it doubles as the hint
adoption for `lazy_init_failed` + `cic_configuration`.

---

## 10. Score

**91 / 100** (±0 vs r8's 91)

**What earned the +1.5 this cycle:**

- **`7d0bc4c5` (cold-init code unification)** — closes r8 §3 primary
  MED. The substring-match-class fragility ("same cause, different
  codes") is now fully eliminated at the cold-init path. Three call
  sites of `init_pool_async()` now stamp one canonical code
  (`lazy_init_failed`). Real **+1.0**.
- **`389749ca` (backend-not-init unification)** — closes r8 §3 case
  4 AND r8 §6 spelling drift. Three call sites of
  `context.backend() == None` stamp one canonical code
  (`backend_not_initialized`) with one canonical spelling. Real
  **+0.5**.

**What earned the -1.5 this cycle (offsetting):**

- **§3 carry — `not_configured` still overloads 2 conditions across
  4 sites.** Narrower than r8's MED (4 conditions, 5/6 sites) but
  not closed. Pool-slot-empty (invariant breach) and `db_url` unset
  (operator config) share a code; the SDK cannot distinguish "this
  should never happen, file a bug" from "set your env var". Real
  **-0.5**.
- **§4 NEW — `config_hinted()` + `validation_hinted()` both
  zero-callered.** The hygiene gap is cosmetic for the wire shape
  but undercuts the rail's self-documentation claim — and r8 §4
  INFO read the situation incorrectly (claimed `config_hinted` had
  2 callers; it has 0). Real **-0.5**.
- **§6 carry — `finalise_backfill` warn missing `name` +
  `collection`** for the 5th cycle in a row. The 2-LOC fix has been
  on the rank list since r6; not landing is a discipline marker on
  observability hygiene. Real **-0.5**.

**Net cycle: +1.5 - 1.5 = 0.** Score holds at 91.

**Why not higher (the -9 deficit, refreshed):**

- §3 residual overload — **+1.0** when disambiguated (either split
  the code or add hints to differentiate).
- §2.LOW carry (4th cycle, 5 P0001 codes hint-less) — **+1.0** when
  `validation_hinted` adoption lands (or constructor deletion +
  inline struct literal documentation).
- §6 finalise_backfill — **+0.5** observability.
- §8 #5 audit alphabet — **+0.25** polish.
- §4 NEW constructor zero-caller hygiene — **+0.25** cleanup.
- §7 prefix_message wildcard — **+0.5** defensive.
- Cross-crate SDK gaps (`toJSON` hint drop + `withRetry` predicate
  narrowness) — block ceiling to ~96. Out of scope but tracked.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87) → r7 (91) → r8 (91)
→ r9 (91).

**Plateau confirmed at 91 for 3 consecutive cycles.** The native
rail's structural work is complete: the 4 r5 MAJORs all closed; the
substring-match anti-pattern eliminated; the Configuration hint slot
first-class; the two prior MED-NEW drift findings from r8 closed
this cycle. What remains is:

1. Residual code overload on `not_configured` (LOW-MED, narrower
   than before).
2. Hint adoption across ValidationFailed (4th-cycle carry).
3. Two zero-callered constructors (hygiene only).
4. Observability metadata in `finalise_backfill` warn (5th-cycle
   carry).
5. Cross-crate `toJSON`/`withRetry` (out of native scope).

**Forward projection (per r8 §10):** if §3 disambiguation (~6 LOC)
+ §2.LOW `validation_hinted` adoption (~25 LOC) + §6
`finalise_backfill` (~2 LOC) all land next cycle, the native rail
should reach **93-94 / 100**. Anything higher requires either the
SDK-side `toJSON`/`withRetry` motion or a substantively new wire
feature (e.g., `retryable: bool` flag on `OpError`).

The r8 plateau prediction was correct: cycle 09:30 said "if §3 +
§2.LOW land next cycle". §3 partially landed (primary MED + case 4
+ spelling closed; residual 2-way overload remains); §2.LOW did not.
Net zero is the honest answer. The plateau signal at 91 is
self-consistent across r7/r8/r9 — the codebase is at the asymptote
for what native-rail-only changes can yield without SDK-side motion
or new wire features.
