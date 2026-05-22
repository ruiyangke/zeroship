# plugin-db Error-UX Review — 2026-05-22 r12

Scope: `crates/plugin-db/src/` at HEAD `6e54ebb9` (cycles 14:47 +
15:17 + 15:47 landed). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r11.md` (94.0 / 100).

Lens: SDK-author `.code` / `.message` / `.hint` clarity at every
fallible boundary + verify the unified F1 warn-shape contract across
the (now) 7 sites + verify the new `validation_refused` terminal
status reaches JS cleanly without leaking the operator-side audit-row
state into the SDK surface.

---

## TL;DR — what landed since r11

**Resolved (or de-novo):**

- **[r11 §6 #4 — CLOSED] `audit.rs:818` `invalid_app_id` alphabet.**
  At HEAD `audit.rs:875-880` the message reads `"audit: invalid
  app_id: {name} (allowed: ASCII alphanumeric + underscore +
  hyphen)"`. Mirrors the `validate_field_name` shape and the
  `replication.rs:91-97` exemplar. **5-cycle carry, closed.** **+0.25.**
- **[r11 §4 NEW LOW — CLOSED] F1 warn-shape `tracing-subscriber`
  capture harness.** `0bf71f27` added `crates/plugin-db/src/
  test_support/mod.rs` (capture layer) gated `#[cfg(test)]`, plus 11
  new unit tests pinning the F1 / [I6] / [I23] warn-shape contracts.
  The shape is no longer reviewer-enforced only — a contributor
  renaming `audit_err` → `error` would fail
  `f1_warn_shape_documentation_snapshot` at unit-test time. **+0.5.**
- **[NEW] F1 warn-half family now 7 sites with `audit_err`,
  `transition` discriminator, `app_id` display field.** Cycle 15:47
  `d07616a2` rebased `validate.rs:101-108`'s `tracing::warn!(error =
  ?e, "audit: failed to log destructive op")` onto the F1 grammar:
  `app_id = %ctx.app_id, collection = %op.collection, transition =
  "ValidationRefused/insert_failed", audit_err = %audit_err`. The
  message body still differs from the other 6 (this is the audit
  INSERT, not an `update_audit_status`), but the **operator-grep
  contract on field names is now uniform across 7 sites**. Real **+0.5.**
- **[NEW SDK-VISIBLE] `validation_refused` terminal status flow.**
  Cycle 15:47 `6afab751` introduced `ValidationRefused` in both
  `InitialStatus` and `TerminalStatus` enums (`audit.rs:121-176`),
  extended the audit table's status CHECK constraint via a
  `DROP+ADD` idempotent migration (`audit.rs:264-289`), and pivoted
  `validate.rs:90` to INSERT destructive-op audit rows directly
  terminal (eliminating the orphan-Pending window the prior
  `14d7608f` Failed+marker shape carried). **SDK-facing impact
  audited below in §3.** **+0.25.**

**Carry from r11 (still open at HEAD `6e54ebb9`):**

- **r10 §3 — `not_configured` 4-site overload.** Verified at HEAD:
  `exec.rs:68` / `:321`, `transaction.rs:147`, `auto_tx.rs:199`. **5th
  cycle in carry.**
- **r10 §4 — `config_hinted()` + `validation_hinted()` zero callers.**
  Verified at HEAD: only 3 hits (1 doc + 2 declarations) in
  `error.rs`. **5th cycle in carry.**
- **r10 §8 — `auth/bootstrap.rs:1083-1093` no-op-prefix pin missing
  `backend_not_initialized` + `lazy_init_failed`.** Verified at HEAD:
  still 7 entries, neither code added. **4th cycle in carry.**
- r7 §7 `prefix_message` wildcard arm — unchanged.
- Cross-crate `toJSON` drops `hint` — out of native scope, 7th cycle.
- Cross-crate `withRetry` predicate narrow — out of native scope,
  7th cycle.

**New findings in r12:**

- **[NEW INFO] §3 — `validation_refused` reaches JS via two distinct
  routes, both correctly stamped.**
  1. **Envelope route** (`register_model_dispatch` →
     `DbError::SchemaRefused { code: "validation_refused",
     envelope_json }` → `to_op_error` →
     `OpError::coded("validation_refused", envelope_json, None)`).
     The SDK sees `err.code === "validation_refused"` + message body
     equal to the JSON envelope. **Unchanged from prior cycles; still
     correct.**
  2. **Audit-row route** (`migrations.status()` returns rows with
     `status = 'validation_refused'`). This is the operator-facing
     side and is **deliberately invisible to the SDK error
     boundary** — the JS caller of `register_model` gets the
     `validation_refused` envelope; the audit row is what
     operators query out-of-band. No SDK contract drift. Confirmed.
  Both routes use the same `.code` token (`validation_refused`).
  An operator who greps the audit table by `status =
  'validation_refused'` and an SDK caller who branches on
  `e.code === "validation_refused"` are looking at the same event
  through different windows. **No deduction.**
- **[NEW INFO] §4 — validate.rs warn site uses `collection` instead
  of `audit_id`.** Acceptable: the audit row is being INSERTED, so
  no id exists yet. The collection name + the `transition =
  "ValidationRefused/insert_failed"` discriminator give operators
  enough to pinpoint the row that failed to land. Listed for
  awareness — the warn-shape is *semantically* uniform (4 fields:
  app_id + identifier + transition + audit_err) even though the
  identifier slot varies by site.
- **[NEW LOW] §5 — `ValidationRefused/insert_failed` discriminator
  is the 6th unique `transition=` value.** The 7 F1 sites now emit
  `Applied`, `Failed`, `Failed/invalid_index`, `Failed/data_violation`,
  `Failed/index_build`, `?terminal` (Debug), and
  `ValidationRefused/insert_failed`. The runbook grep
  `transition=Failed/` still matches the 3 backend `Failed/*`
  cases; `transition=ValidationRefused/` would match the new site.
  No collision risk — listed as informational.

**Net score delta vs r11:** see §9.

---

## 1. Verify cycle 14:47 / 15:17 / 15:47 closures end-to-end

### [PASS] `02ead3f4` — `audit.rs:818` alphabet (5-cycle carry closed)

```rust
return Err(DbError::validation(
    "invalid_app_id",
    format!(
        "audit: invalid app_id: {name} (allowed: ASCII alphanumeric + underscore + hyphen)"
    ),
));
```

Reads correctly. Matches `validate_field_name`'s in-message alphabet
hint. The SDK consumer now sees an actionable error: the JS message
includes both the rejected input AND the rule. **PASS.**

### [PASS] `0bf71f27` — `tracing-subscriber` capture layer + 11 tests

Adds `crates/plugin-db/src/test_support/mod.rs` with a `capture()`
helper that returns `(T, Vec<CapturedEvent>)`. Tests pin:
- F1 warn-shape (`f1_warn_shape_documentation_snapshot` at
  `apply.rs:530`) — 4-field contract `app_id` / `audit_id` /
  `transition` / `audit_err` + message body.
- `destructive_invariant_error` ERROR-shape (3 fields:
  `change_kind` / `class` / `collection`).
- `[I6] release_advisory_lock` warn-shape (3 fields: `app_id` /
  `name` / `error`).
- `[I23] mig_lock` shadow-replace error-shape (4 fields:
  `prev_name` / `prev_audit_id` / `new_name` / `new_audit_id`).

The harness is `#[cfg(test)]`-gated only (not exposed via the
`test-helpers` Cargo feature — `tracing-subscriber` is dev-deps and
hiding it from the release graph is the right call). Documented
caveat: integration tests in `crates/plugin-db/tests/integration.rs`
cannot use the harness today; end-to-end warn-shape coverage there
needs a separate rebuild. Acceptable trade-off. **PASS.**

### [PASS] `14d7608f` — F2 partial (superseded by `6afab751`)

`14d7608f` was the intermediate "destructive-op Failed + marker"
shape that wrote `Pending → Failed` with a marker column. The
second-write path emitted a warn on UPDATE failure. Superseded one
cycle later by `6afab751` (terminal-on-INSERT), so the marker shape
is no longer reachable at HEAD. Verified by reading
`validate.rs:90` — `InitialStatus::ValidationRefused`, not
`InitialStatus::Pending`. **No carry; superseded clean.**

### [PASS] `6afab751` — F2 upgrade to `ValidationRefused` terminal

Three coordinated changes:
1. `audit.rs:121-149` — new `InitialStatus::ValidationRefused` +
   `as_sql() = "validation_refused"`.
2. `audit.rs:151-176` — new `TerminalStatus::ValidationRefused` +
   `as_sql() = "validation_refused"`.
3. `audit.rs:240-261` — status CHECK widened on existing tables via
   `DROP CONSTRAINT IF EXISTS … ADD CONSTRAINT` idempotent rewrite.
   New CREATE TABLE inlines the wider list at line 240.
4. `validate.rs:90` — INSERT directly with `ValidationRefused`
   terminal status; eliminates the orphan-Pending window.

**SDK-visible surface:** the envelope-route already returned
`.code = "validation_refused"` (unchanged from prior cycles). The
new terminal status is operator-side only — `migrations.status()`
now returns `status = "validation_refused"` for destructive-op
refusals (was `"pending"` or `"failed"` before). This is a NEW
operator-visible SDK surface IF the platform exposes a
`migrations.status()` query to creators; if it's operator-only the
SDK is unaffected.

**Checked end-to-end:**
- Envelope dispatch at `orchestrator/register_model/mod.rs:205-208`:
  ```rust
  .map_err(|envelope_json| DbError::SchemaRefused {
      code: "validation_refused",
      envelope_json,
  })
  ```
  → `error.rs:228-237` `to_op_error()` stamps `.code =
  "validation_refused"` + message = envelope body. **Correct.**
- Status CHECK accepts `'validation_refused'` for both INITIAL
  (INSERT path from validate.rs) and TERMINAL (transition path,
  reserved for future symmetry). **Correct.**
- `update_audit_status` docstring updated to list
  `validation_refused` as an accepted terminal (`audit.rs:369-374`).
  **Correct.**

**PASS — no SDK regression.** The new terminal flows the same
.code as the envelope; operators see a new audit-row status
that's grep-distinguishable from `'failed'` (closing the
"refused vs failed" ambiguity).

### [PASS] `d07616a2` — validate.rs warn-shape drift fix + audit.rs:368 docstring

`d07616a2` (cycle 15:47) lined up the warn site in `validate.rs:101`
with the F1 family grammar:

```rust
if let Err(audit_err) = backend.write_audit_row(&ctx.app_id, &row).await {
    tracing::warn!(
        app_id = %ctx.app_id,
        collection = %op.collection,
        transition = "ValidationRefused/insert_failed",
        audit_err = %audit_err,
        "audit: failed to log destructive op",
    );
}
```

This is the 7th F1 family site. The `audit.rs:368` docstring
expanded the allowed terminal list (Applied / AppliedWithDeadLetter
/ Failed / Cancelled / ValidationRefused). **PASS.**

---

## 2. Verify r11 carry-overs at HEAD `6e54ebb9`

### [CARRY] r10 §3 — `not_configured` overload

Grep at HEAD returns 4 production sites — unchanged.
**5th cycle in carry.** 6-LOC fix.

### [CARRY] r10 §4 — zero-caller constructors

Grep at HEAD: 3 hits (1 doc pointer + 2 declarations). No
production callers.
**5th cycle in carry.** 12-or-30 LOC fix (adopt or delete).

### [CLOSED] r11 §6 #4 — `audit.rs:818` missing alphabet — see §1.

### [CARRY] r10 §8 — no-op-prefix pin missing 2 codes

Verified at `auth/bootstrap.rs:1083-1093` at HEAD: 7 entries,
`backend_not_initialized` and `lazy_init_failed` both absent.
**4th cycle in carry.** 2-LOC fix.

### [CLOSED] r11 §4 NEW — tracing-subscriber dev-dep unused — see §1.

---

## 3. `validation_refused` SDK surface — end-to-end audit

| Surface | Source | `.code` / `.status` | SDK-visible? |
|---|---|---|---|
| `DbError::SchemaRefused.code` | `register_model/mod.rs:205-208` | `"validation_refused"` static | YES — `e.code` |
| `OpError` message body | `error.rs:236` | envelope JSON | YES — `JSON.parse(e.message)` |
| Audit row `status` column | `validate.rs:90` (INSERT) | `'validation_refused'` | YES IFF SDK surfaces `migrations.status()` |
| `InitialStatus::ValidationRefused.as_sql()` | `audit.rs:144` | `"validation_refused"` | indirect (audit row) |
| `TerminalStatus::ValidationRefused.as_sql()` | `audit.rs:173` | `"validation_refused"` | indirect (audit row) |

All five surfaces use the **same token** (`validation_refused`). An
operator running `SELECT * FROM __zeroship_migrations WHERE status
= 'validation_refused'` and a JS caller branching on
`e.code === "validation_refused"` are observing the same event.

**Verdict: clean.** The `.code` contract is uniform across the
envelope dispatch and the operator-side audit table.

---

## 4. F1 warn-half family — 7 sites at HEAD

| Site | `app_id` | id-field | `transition` | `audit_err` | Extra |
|---|---|---|---|---|---|
| `apply.rs:178-184` (Applied) | `%app_id` | `audit_id = id` | `"Applied"` | `%audit_err` | — |
| `apply.rs:209-216` (Failed) | `%app_id` | `audit_id = id` | `"Failed"` | `%audit_err` | `ddl_err` |
| `backend/postgres.rs:496-502` (invalid_index) | `%app_id` | `audit_id = id` | `"Failed/invalid_index"` | `%audit_err` | `attempt` |
| `backend/postgres.rs:548-554` (data_violation) | `%app_id` | `audit_id = id` | `"Failed/data_violation"` | `%audit_err` | `sqlstate` |
| `backend/postgres.rs:597-604` (index_build) | `%app_id` | `audit_id = id` | `"Failed/index_build"` | `%audit_err` | `attempt, transient` |
| `migrations.rs:652-662` (finalise_backfill) | `%app_id` | `audit_id = audit_id` | `?terminal` | `%audit_err` | `name, collection` |
| **`validate.rs:101-108` (validate-INSERT failed) — NEW** | `%ctx.app_id` | `collection = %op.collection` | `"ValidationRefused/insert_failed"` | `%audit_err` | — |

**Field-name contract uniform across 7 sites:** every site has
`app_id` + a stable identifier field + `transition` + `audit_err`.
The identifier field varies: 5 sites use `audit_id`, 1 uses
`audit_id` + `name`+`collection`, and the new validate site uses
`collection` (because no audit id exists pre-INSERT).
Message body splits 6+1: the `update_audit_status`-family sites
share `"update_audit_status failed; row stays in 'running' until
reset"`; finalise_backfill has its own remediation prose; validate
uses `"audit: failed to log destructive op"`. Acceptable —
different operations, distinct remediation hints.

**Snapshot pinning:** `f1_warn_shape_documentation_snapshot`
(`apply.rs:530-585`) pins the 4-field contract at unit-test time
under `0bf71f27`'s capture layer. It pins the **`apply.rs:178`
shape verbatim**; the new validate site uses `collection` instead
of `audit_id`, which the snapshot test does NOT directly cover.
That's a minor gap — the validate site could drift its identifier
field without the existing snapshot complaining. Listed in §6.

---

## 5. Sample 4 NEW or REFINED sites — clarity audit

### #1 — `validate.rs:101-108` — best-effort audit INSERT (NEW)

```rust
tracing::warn!(
    app_id = %ctx.app_id,
    collection = %op.collection,
    transition = "ValidationRefused/insert_failed",
    audit_err = %audit_err,
    "audit: failed to log destructive op",
);
```

7th F1 family member. Field shape matches; message body
distinguishes the INSERT failure from the
`update_audit_status` family. The `transition =
"ValidationRefused/insert_failed"` is the only place this exact
discriminator appears, so a runbook grep on it pinpoints this
single site. **PASS.**

### #2 — `audit.rs:875-880` — invalid_app_id alphabet (CLOSED carry)

```rust
return Err(DbError::validation(
    "invalid_app_id",
    format!(
        "audit: invalid app_id: {name} (allowed: ASCII alphanumeric + underscore + hyphen)"
    ),
));
```

5-cycle carry, finally closed. The SDK consumer sees both the
rejected input AND the rule in `e.message`. Symmetric with
`validate_field_name`'s shape. **Exemplar.**

### #3 — `audit.rs:144` / `audit.rs:173` — `ValidationRefused` enum

```rust
pub enum InitialStatus { Pending, Running, ValidationRefused }
pub enum TerminalStatus { Applied, AppliedWithDeadLetter, Failed,
                          Cancelled, ValidationRefused }
```

Symmetric: same SQL token (`"validation_refused"`) on both sides.
The docstring on `InitialStatus::ValidationRefused` explicitly
names the rationale (orphan-Pending elimination). Operators
querying `__zeroship_migrations` can grep `status =
'validation_refused'` and match exactly the rows refused by
validate. **Exemplar.**

### #4 — `audit.rs:264-289` — idempotent CHECK constraint widening

```rust
ALTER TABLE … DROP CONSTRAINT IF EXISTS __zeroship_migrations_status_chk
ALTER TABLE … ADD CONSTRAINT __zeroship_migrations_status_chk CHECK (
    status IN ('pending','running','applied','applied_with_dead_letter',
               'failed','cancelled','rolled_back','validation_refused')
)
```

DROP+ADD pattern. Idempotent across three table-state branches
(fresh CREATE TABLE / old constraint exists / no constraint at
all). The fail mode is well-shaped: a `coded_sql` wrap on either
DDL gives the operator a typed `.code` if Postgres refuses the
ALTER (e.g. concurrent migration holds an incompatible lock).
**PASS — well-defended.**

**Sample observations:**
- 4 of 4 PASS. The cycle's biggest delta is the new
  `validation_refused` terminal status, which is correctly
  threaded through the wire token / SDK contract / operator-side
  audit table without leaking the operator-side state into the
  SDK error envelope.

---

## 6. Concrete fixes ranked by SDK-author impact (r12)

| Rank | Finding | LOC | Files | Status vs r11 |
|---|---|---|---|---|
| 1 | r7 §10#1 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts:46-65` | UNCHANGED (7th carry) |
| 2 | r7 §2.LOW — adopt `validation_hinted()` for 5 `session_*` codes | ~25 | `auth/session.rs:259` | UNCHANGED (7th carry) |
| 3 | r10 §3 — disambiguate residual `not_configured` overload | ~6 | `exec.rs:68,:321`, `transaction.rs:147`, `auto_tx.rs:199` | UNCHANGED (5th carry) |
| 4 | r10 §4 — adopt `config_hinted()` at 2 struct-literal sites OR delete | ~12 / ~30 | `wal_consumer.rs:349-358`, `replication.rs:277-286`, `error.rs:311-343` | UNCHANGED (5th carry) |
| 5 | r10 §8 — add `backend_not_initialized` + `lazy_init_failed` to no-op-prefix pin | ~2 | `auth/bootstrap.rs:1083-1093` | UNCHANGED (4th carry) |
| 6 | r12 §4 NEW LOW — extend `f1_warn_shape_documentation_snapshot` to cover the validate-INSERT identifier-slot variant | ~25 | `orchestrator/register_model/validate.rs` (new test) | NEW (1st carry) |
| 7 | r6 §10#8 — augment `migrations.rs:329` "returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs` | UNCHANGED |
| 8 | r6 §6 / r7 §7 — replace `_ => {}` in `prefix_message` with explicit arms | ~10 | `error.rs:386` | UNCHANGED |
| 9 | r6 §10#10 — stamp `retryable: true` wire flag on `OpError::coded` | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 10 | r7 §10#4 — SDK `withRetry` predicate add 3 retryable codes | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 11 | r6 §10#11 — route `migrations::coded()` through `DbError::Coded` or delete | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

---

## 7. Retry semantics — unchanged from r11

| `.code` | Variant | Auto-retry? | Hint? | r12 verdict |
|---|---|---|---|---|
| `transient`, `serialization_failure`, `lock_not_available` | Transient/Serialization/LockContention | YES | YES | ✓ unchanged |
| `unique_violation` / `fk_violation` / `not_null_violation` / `check_violation` | constraint variants | NO | NO | ✓ unchanged |
| `validation_refused` | SchemaRefused | NO | envelope IS message | ✓ unchanged (NEW: also visible in audit-row `status` column) |
| `not_provisioned`, `wal_level_not_logical` | Configuration | NO | YES | ✓ unchanged |
| `lazy_init_failed`, `cic_configuration` | Configuration | NO | NO | ✓ unchanged |
| `not_configured` | Configuration | NO | NO | ⚠ overloads 2 conditions (r10 §3, 5th cycle carry) |
| `backend_not_initialized` | Configuration | NO | NO | ✓ unchanged |
| `invalid_identifier` | ValidationFailed via QueryError | NO | NO (message is remediation) | ✓ unchanged |
| `invalid_app_id` | ValidationFailed | NO | NO (message NOW carries alphabet rule) | ✓ improved this cycle |

Pin at `error.rs:646-662` unchanged.

---

## 8. Score

**95.5 / 100** (+1.5 vs r11's 94.0)

**What earned the +1.5 this cycle:**

- **`0bf71f27` (tracing-subscriber capture layer + 11 tests)** —
  closes the r11 §4 NEW LOW with a real harness, not a stub. The
  F1 warn-shape contract is now compile-time-equivalent for the
  apply.rs site and unit-tested for the [I6] / [I23] / destructive-
  invariant ERROR shapes. Real **+0.5.**
- **`6afab751` (ValidationRefused terminal status)** — eliminates
  the orphan-Pending window that the prior Failed+marker shape
  carried, and introduces a NEW SDK-grep-distinguishable audit-row
  status (`'validation_refused'`) symmetric with the existing
  envelope `.code`. The operator-side and SDK-side both reach for
  the same token. Real **+0.25.**
- **`02ead3f4` (audit.rs:818 alphabet)** — closes the 5-cycle
  error-ux carry. Inline alphabet rule mirrors the
  `validate_field_name` exemplar. Real **+0.25.**
- **`d07616a2` (validate.rs warn-shape drift fix + audit.rs:368
  docstring)** — 7th F1 family site lined up on the operator-grep
  contract; the cycle's drift was caught in-flight (cycle 15:47)
  rather than landing in production. Real **+0.5.**

**What held the cycle back (no offsets, just unspent slack):**

- r10 §3 `not_configured` overload — 5th cycle, no movement.
- r10 §4 zero-caller constructors — 5th cycle, no movement.
- r10 §8 pin ratchet — 4th cycle, no movement.
- r12 §4 NEW LOW: F1 snapshot test pins the `apply.rs` identifier-
  slot shape (`audit_id`) but doesn't directly cover the validate-
  INSERT site's `collection` slot variant. +0.25 available next
  cycle.

**Why not higher (the -4.5 deficit, refreshed):**

- r10 §3 disambiguate `not_configured` — **+0.5–1.0** when split.
- r9 §2.LOW `validation_hinted` adoption (5 P0001 codes) — **+1.0.**
- r10 §4 constructor zero-caller adoption-or-delete — **+0.25.**
- r10 §8 no-op-prefix pin ratchet — **+0.25.**
- r12 §4 NEW LOW snapshot extension — **+0.25.**
- r7 §7 `prefix_message` wildcard arm — **+0.5** defensive.
- Cross-crate SDK gaps (`toJSON` hint drop + `withRetry` predicate
  narrowness) — block ceiling to ~97. Out of native scope.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87) → r7 (91) → r8 (91)
→ r9 (91) → r10 (92.5) → r11 (94.0) → r12 (95.5).

**Three consecutive +1.5 cycles.** The plateau is decisively
broken. This cycle landed:
- Two long-running carries (audit.rs:818 alphabet, 5-cycle;
  tracing-subscriber snapshot pin, 1-cycle but a r11 deliberate
  hold).
- A SDK-visible terminal status (`validation_refused`) threaded
  cleanly from envelope `.code` through audit-row `status` column.
- A 7th F1 warn-half site lined up on the grammar in-cycle.

**Forward projection:** if the next cycle lands ANY two of:
(a) §3 `not_configured` split (6 LOC),
(b) §8 no-op-prefix pin ratchet (2 LOC),
(c) §6 #6 snapshot extension to validate-INSERT shape (25 LOC),
(d) zero-caller constructor adoption (`config_hinted` for 2 sites)
— the native rail should clear **96.5 / 100**. The 4 candidates
total ~70 LOC, achievable in one sitting. The ~97 ceiling without
cross-crate work is now 1 cycle out at current +1.5/cycle pace.
