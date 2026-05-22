# plugin-db Error-UX Review — 2026-05-22 r10

Scope: `crates/plugin-db/src/` at HEAD `71a457a1` (cycles 11:17 + 12:17
landed). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r9.md` (91 / 100).

Lens: SDK-author error-handling discipline (`.code` / `.message` /
`.hint` clarity) + new this cycle: grep-friendliness of operator-
facing `tracing::warn!` shapes (3 commits this cycle added warns).

---

## TL;DR — what landed since r9

**Resolved (or de-novo):**

- **[I12] `validate_field_name` non-ASCII rejection (`403b3891`)** —
  the new `QueryError::InvalidIdent` message names the alphabet
  inline: `"invalid field name: {name} (allowed: ASCII alphanumeric +
  underscore)"`. Maps via `From<QueryError> for DbError` to `.code =
  "invalid_identifier"` (stable). The message-as-remediation pattern
  matches `replication.rs:91-97`'s `invalid_app_id` exemplar (r9 §6
  #3). Two new unit tests pin the negative + positive shape. **PASS
  outright** — the SDK consumer sees a stable code and a message that
  IS the remediation. **+0.5.**
- **[I6] `release_advisory_lock` returns `Result` (`51c342e8`)** —
  trait signature is now `Result<(), DbError>`; impl lifts PG errors
  via `DbError::from_pg` (so SQLSTATE classification is preserved if
  the caller ever switches to typed-match). The two production
  callers warn-and-continue. **Warn shape distinguishability check:**
  cancelled-refusal path emits `"release_advisory_lock failed on
  cancelled-refusal path (lock auto-releases on session end)"`;
  backfill-finalise path emits `"release_advisory_lock failed on
  backfill-finalise path (lock auto-releases on session end)"`. The
  caller-context substring (`cancelled-refusal` vs `backfill-finalise`)
  is grep-distinguishable. Both carry `app_id` and `name` structured
  fields. **PASS.** **+0.25.**
- **[F1 warn-half] 5 audit-status update sites now warn (`fcf7ce3c`)**
  — re-walked all 5 message shapes. Each names the audit row
  (`audit_id`), the app (`app_id`), and the failing transition (in
  the message body: `(Applied)` / `(Failed)` / `(Failed/invalid_index)`
  / `(Failed/data_violation)` / `(Failed/index_build)`). The
  `apply.rs` pair (lines 176-181 + 203-209) further attaches the
  caller-state remediation hint inline: `"row stays in 'running'
  until reset"`. The `backend/postgres.rs` trio attaches retry-loop
  context (`attempt`, `transient`, `sqlstate`). The `apply.rs:Failed`
  site additionally carries both `ddl_error` AND `audit_error` so the
  operator can tell which side failed. **PASS.** All 5 sites would
  genuinely help an operator. **+0.75.**

**Carry from r9 (still open at HEAD `71a457a1`):**

- **r9 §3 — `not_configured` overloads 2 conditions across 4 sites.**
  Verified at HEAD: 4 production literal sites
  (`exec.rs:68`/`:321`, `transaction.rs:147`, `auto_tx.rs:199`).
  Pool-slot-empty (invariant breach) vs `db_url` unset (operator
  config) still share `.code = "not_configured"`. **2nd cycle in
  carry.**
- **r9 §4 — `config_hinted()` + `validation_hinted()` zero callers.**
  Verified at HEAD: only 3 hits (1 doc pointer, 2 declarations) in
  `error.rs`. No production sites. **2nd cycle in carry.**
- **r9 §5 — `finalise_backfill` warn missing `name` + `collection`.**
  Verified at HEAD `migrations.rs:647-655`. The
  `lock_snapshot()` destructure at `:458` still binds `name` and
  `collection` in lexical scope; the warn at `:647-655` carries only
  `app_id` / `audit_id` / `terminal` / `error`. **6th cycle in
  carry** (the new `release_advisory_lock` warn 12 lines below
  *does* carry `name`, making the omission look pointed). 2-LOC fix
  unchanged.
- **r9 §6 #4 — `audit.rs:816-819` `invalid_app_id` message missing
  alphabet.** Verified at HEAD `audit.rs:816-819`: still `"audit:
  invalid app_id: {name}"`, no alphabet. Inferior to the
  `replication.rs:91-97` twin (`" — only [A-Za-z0-9_] permitted"`)
  AND now inferior to the brand-new `query.rs:131-133` site landed
  this cycle (`"(allowed: ASCII alphanumeric + underscore)"`).
  **3rd cycle in carry, +1 fresh peer.**
- **r9 §8 `auth/bootstrap.rs:1083-1090` no-op-prefix pin.** Verified
  at HEAD: still 7 entries, still missing both
  `"backend_not_initialized"` (unified at r9's `389749ca`) and
  `"lazy_init_failed"` (unified at r9's `7d0bc4c5`). Two cycles of
  rail-side closures the pin failed to ratchet up. **1st cycle in
  carry.**
- **r9 §7 retry-table** — unchanged. Pinned correctly by
  `retryable_variants_carry_hint`.
- **Cross-crate `toJSON` drops `hint`** — out of native scope, 5th
  cycle.
- **Cross-crate `withRetry` predicate narrow** — out of native scope,
  5th cycle.

**New findings in r10:**

- **[NEW INFO] `release_advisory_lock` warn texts share a stem.**
  Both warns start `"release_advisory_lock failed on "` with the
  caller-context substring after. Good for `grep
  release_advisory_lock` (one query finds both); equally good for
  `grep cancelled-refusal` or `grep backfill-finalise` (each query
  finds one). No improvement needed. Listed only because it's the
  rail's first deliberate caller-discriminator-in-message-body
  pattern; future warns should follow.
- **[NEW LOW] F1 warn-half `audit.rs` site missing.** The grep
  `grep -n 'let _ = .*update_audit_status' crates/plugin-db/src/` at
  HEAD returns zero hits — `fcf7ce3c` converted 5 of the historical
  `let _ = update_audit_status` sites. Sanity-check confirms all 5
  documented in the commit message map to live warns at HEAD. **No
  silent swallow sites remain for `update_audit_status` results.**
  PASS. (No deduction — this is a positive finding placed in the new-
  findings list because the verification surprised me.)
- **[NEW LOW] I12 message says "ASCII alphanumeric + underscore"
  but `validate_app_id` accepts hyphens** — semantic difference is
  correct (app_id is a schema name + UUIDs have hyphens; field names
  are columns and shouldn't carry hyphens because hyphenated
  identifiers need quoting). The two message texts at
  `query.rs:131-133` ("ASCII alphanumeric + underscore") vs the
  `audit.rs:818` twin (which SHOULD say "ASCII alphanumeric +
  underscore + hyphen") are factually correct for their respective
  domains. **No action needed**, but if `audit.rs:816-819` is ever
  fixed per r9 §6 #4, the rule "alphabet differs from the field-name
  twin" must be respected. Listed for the fixer's benefit.

**Net score delta vs r9:** see §9.

---

## 1. Verify cycle 11:17 / 12:17 closures end-to-end

### [PASS] `403b3891` — I12 `validate_field_name` ASCII allowlist

`query.rs:127-134`:

```rust
if !name
    .chars()
    .all(|c| c.is_ascii_alphanumeric() || c == '_')
{
    return Err(QueryError::InvalidIdent(format!(
        "invalid field name: {name} (allowed: ASCII alphanumeric + underscore)"
    )));
}
```

- `.code`: maps via `From<QueryError> for DbError` at `error.rs:484`
  to `"invalid_identifier"` (stable static).
- Message: names the offending value AND the valid alphabet inline
  (parenthetical pattern matches `replication.rs:91-97` shape).
- Doc comment at `query.rs:102-110` explains the *why* (truncation
  aliasing) so a future maintainer doesn't relax the check.
- Tests at lines 4283-4304 pin both directions.

The user asked: "does the new InvalidIdent error message clearly
tell the SDK what allowed alphabet is?" Answer: **yes, inline in
the message body.** The text is concrete (`ASCII alphanumeric +
underscore`), not abstract ("invalid characters"). An SDK consumer
who logs `e.message` sees the rule. **PASS.**

### [PASS] `51c342e8` — I6 `release_advisory_lock: Result`

Trait at `backend/mod.rs:159-164`:

```rust
async fn release_advisory_lock(
    &self,
    client: &Self::Client,
    key1: &str,
    key2: &str,
) -> Result<(), DbError>;
```

Impl at `backend/postgres.rs:157-169` lifts via `DbError::from_pg`.

The two production callers in `migrations.rs`:

**Caller #1 — cancelled-refusal path (`:284-295`):**
```rust
if let Err(e) = backend
    .release_advisory_lock(&client, &lock_key, name)
    .await
{
    tracing::warn!(
        app_id,
        name,
        error = %e,
        "release_advisory_lock failed on cancelled-refusal path (lock auto-releases on session end)",
    );
}
```

**Caller #2 — backfill-finalise path (`:658-669`):**
```rust
if let Err(e) = backend
    .release_advisory_lock(&client, &lock_key, &name)
    .await
{
    tracing::warn!(
        app_id,
        name,
        error = %e,
        "release_advisory_lock failed on backfill-finalise path (lock auto-releases on session end)",
    );
}
```

The user asked: "Are the warn messages distinguishable
(cancelled-refusal vs backfill-finalise)?" Answer: **yes** — the
caller-context substring (`cancelled-refusal path` vs
`backfill-finalise path`) is grep-distinct. Both share the prefix
`"release_advisory_lock failed on "` so a single grep finds both;
a more specific grep finds one. **PASS.**

Structured fields: both carry `app_id`, `name`, `error`. Same
field-set across callers — uniform shape, easy to ingest into a log
pipeline. The auto-release-on-session-end remediation is in the
message body so a stuck operator sees it without reading the source.

### [PASS] `fcf7ce3c` — F1 warn-half (5 sites)

Walked all 5. The user asked: "Verify the messages would actually
help an operator: do they name the audit row, the app, the failing
transition?"

| Site | `app_id` | `audit_id` | Transition (in message) | Extra context |
|---|---|---|---|---|
| `apply.rs:176-181` (Applied terminal) | yes (`%`) | yes | `(Applied)` | `"row stays in 'running' until reset"` (remediation) |
| `apply.rs:203-209` (Failed terminal) | yes (`%`) | yes | `(Failed)` | both `ddl_error` AND `audit_error` (disambiguates) + remediation |
| `backend/postgres.rs:500-506` (INVALID-index loop) | yes | yes | `(Failed/invalid_index)` | `attempt` |
| `backend/postgres.rs:555-561` (data-violation retry) | yes | yes | `(Failed/data_violation)` | `sqlstate` |
| `backend/postgres.rs:606-613` (transient/non-transient build) | yes | yes | `(Failed/index_build)` | `attempt`, `transient` |

All three checkboxes satisfied across all 5 sites. The transition is
in the message body (e.g. `update_audit_status(Failed/data_violation)
failed`) — searchable as a single token. The retry-loop sites carry
the loop-state context (`attempt` / `transient` / `sqlstate`) which
is genuinely operator-actionable. The `apply.rs:Failed` site is the
most thoughtful: by carrying *both* `ddl_error` and `audit_error`,
the operator can immediately tell whether the primary failure is the
DDL itself (then JS already saw it) or only the audit-write (then JS
saw the DDL error and the row will sit in 'running').

**Verdict: every warn would genuinely help.** PASS.

### [PASS] `71a457a1` — doc-drift cleanup

Preamble only. No error-message change. As described, no error-UX
impact. **PASS.**

---

## 2. Verify r9 carry-overs at HEAD `71a457a1`

### [CARRY] r9 §3 — `not_configured` overload

Grep at HEAD returns 4 production sites for `"not_configured"`:

- `exec.rs:68` — pool-slot empty after init (invariant breach)
- `exec.rs:321` — same (in `ensure_pool`)
- `orchestrator/transaction.rs:147` — `db_url` not set
- `orchestrator/auto_tx.rs:199` — `db_url` not set

Unchanged from r9. **Still LOW-MED**: 2-way overload across 4 sites,
both operator-side (one invariant, one config). 2nd cycle.

### [CARRY] r9 §4 — zero-caller constructors

```
grep -rn 'validation_hinted\|config_hinted' crates/plugin-db/src/
```

returns only 3 hits at HEAD:
- `error.rs:303` (doc pointer in `config()` comment)
- `error.rs:315` (`config_hinted` declaration)
- `error.rs:337` (`validation_hinted` declaration)

No production callers. 2nd cycle. **Still LOW** (hygiene only; wire
shape is identical to struct literals).

### [CARRY] r9 §5 — `finalise_backfill` warn shape

`migrations.rs:647-655` at HEAD still carries only `app_id` /
`audit_id` / `terminal` / `error`. The lexical-scope binding at
`:458` still exists. **6th cycle in carry.** The cycle 11:17 +
12:17 changes added a warn at `:663-668` (the new
`release_advisory_lock` site, 12 lines below) which DOES carry
`name` — the contrast inside the same function makes the omission
more visible.

Still LOW (observability gap). 2-LOC fix unchanged.

### [CARRY] r9 §6 #4 — `audit.rs:816-819` `invalid_app_id` missing alphabet

```rust
return Err(DbError::validation(
    "invalid_app_id",
    format!("audit: invalid app_id: {name}"),
));
```

Unchanged at HEAD. The check at `:812-815` permits
`[A-Za-z0-9_-]` (note the hyphen — UUIDs need it; differs from the
field-name twin). 3rd cycle. The new `query.rs:131-133` site landed
this cycle uses the inline-alphabet pattern, so this carry now has
**two** in-tree exemplars to copy from. 1-LOC fix.

### [CARRY] r9 §8 — no-op-prefix pin in `auth/bootstrap.rs:1083-1090`

Verified at HEAD: 7 entries, includes `wal_level_not_logical` and
`not_configured` and the 5 `session_*` codes. **Missing**:
`backend_not_initialized` (unified r9) and `lazy_init_failed`
(unified r9). The pin's role is to enumerate every structured
variant the prefix-leaves-alone test cares about; two no-op codes
landed since the pin was last touched. 1st cycle in carry — listed
now because it would be trivially gradable next cycle alongside any
real fix.

---

## 3. Sample 5 NEW error/warn sites — clarity audit

Walked 5 sites that landed THIS cycle (different from r9's sample).

### #1 — `query.rs:131-133` — `invalid_identifier` field-name ASCII

Already covered §1. **PASS.** Inline alphabet, stable code, message-
as-remediation.

### #2 — `migrations.rs:289-295` — release_advisory_lock cancelled-refusal warn

Already covered §1. **PASS.** Caller-context in message body,
structured fields for app + name, remediation in message body.

### #3 — `apply.rs:203-209` — F1 audit-write Failed warn (the most thoughtful)

```rust
tracing::warn!(
    app_id = %app_id,
    audit_id = id,
    ddl_error = %msg,
    audit_error = ?upd_err,
    "update_audit_status(Failed) failed; row stays in 'running' until reset",
);
```

Carrying both `ddl_error` and `audit_error` is the rail's first
warn that disambiguates primary-vs-secondary failure in one
structured payload. The message body has the remediation. The
`audit_id` integer is searchable across logs to correlate with the
audit row. **Exemplar.**

### #4 — `backend/postgres.rs:555-561` — F1 data-violation warn

```rust
tracing::warn!(
    app_id,
    audit_id = id,
    sqlstate = code_str,
    error = ?upd_err,
    "update_audit_status(Failed/data_violation) failed",
);
```

The `sqlstate` field is the right axis — an operator triaging a
stuck data-violation knows immediately whether it's `23505` (unique)
or `23502` (not-null) or `23503` (fk) without parsing the inner
error. The `(Failed/data_violation)` token in the message body makes
the transition / cause searchable as one substring.

Minor: the inner `e` (the primary DDL error that caused the
data-violation classification) is consumed by `refuse(...)` two
lines below but NOT carried into this warn. The operator has to
correlate via the audit row (`audit_id`) to see the primary error.
Acceptable but slightly inferior to `apply.rs:203-209`'s shape
(which carries both). **PASS.**

### #5 — `backend/postgres.rs:606-613` — F1 index_build warn

```rust
tracing::warn!(
    app_id,
    audit_id = id,
    attempt,
    transient,
    error = ?upd_err,
    "update_audit_status(Failed/index_build) failed",
);
```

The `transient` boolean is the right signal — operator can tell
whether the retry loop will continue (and the audit-write failure
will be re-attempted next iteration) or whether this is the last
iteration. `attempt` carries the retry count. **PASS.**

**Sample observations:**
- 5 of 5 PASS uniformly. The cycle's warn discipline is markedly
  ahead of the rail's historical median.
- The `apply.rs:203-209` shape (primary + secondary error in one
  warn) is a new pattern worth replicating.

---

## 4. Retry semantics — unchanged from r9

| `.code` | Variant | Auto-retry? | Hint? | r10 verdict |
|---|---|---|---|---|
| `transient`, `serialization_failure`, `lock_not_available` | Transient/Serialization/LockContention | YES | YES | ✓ unchanged |
| `unique_violation` / `fk_violation` / `not_null_violation` / `check_violation` | constraint variants | NO | NO | ✓ unchanged |
| `validation_refused` | SchemaRefused | NO | envelope IS message | ✓ unchanged |
| `not_provisioned`, `wal_level_not_logical` | Configuration | NO | YES | ✓ unchanged |
| `lazy_init_failed`, `cic_configuration` | Configuration | NO | NO | ✓ unchanged |
| `not_configured` | Configuration | NO | NO | ⚠ overloads 2 conditions (r9 §3 carry) |
| `backend_not_initialized` | Configuration | NO | NO | ✓ unchanged |
| `invalid_identifier` (NEW from r10) | ValidationFailed via QueryError mapper | NO | NO (message is remediation) | ✓ correct shape |

Pin at `error.rs:646-662` (`retryable_variants_carry_hint`)
unchanged.

---

## 5. Concrete fixes ranked by SDK-author impact (r10)

| Rank | Finding | LOC | Files | Status vs r9 |
|---|---|---|---|---|
| 1 | r7 §10#1 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts:46-65` | UNCHANGED (5th carry) |
| 2 | r7 §2.LOW — adopt `validation_hinted()` for 5 `session_*` codes | ~25 | `auth/session.rs:259` | UNCHANGED (5th carry) |
| 3 | r9 §3 — disambiguate residual `not_configured` overload | ~6 | `exec.rs:68,:321`, `transaction.rs:147`, `auto_tx.rs:199` | UNCHANGED (2nd carry) |
| 4 | r9 §6 — `finalise_backfill` warn: add `name` + `collection` | 2 | `migrations.rs:647-655` | UNCHANGED (6th carry) |
| 5 | r9 §4 — adopt `config_hinted()` at 2 struct-literal sites OR delete | ~12 / ~30 | `wal_consumer.rs:349-358`, `replication.rs:277-286`, `error.rs:311-343` | UNCHANGED (2nd carry) |
| 6 | r9 §6 #4 — name alphabet inline in `audit.rs:818` | 1 | `audit.rs:818` | UNCHANGED (3rd carry) — now has 2 in-tree exemplars |
| 7 | r10 §8 — add `backend_not_initialized` + `lazy_init_failed` to no-op-prefix pin | ~2 | `auth/bootstrap.rs:1083-1090` | NEW (1st carry) |
| 8 | r6 §10#8 — augment `migrations.rs:329` "returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs` | UNCHANGED |
| 9 | r6 §6 / r7 §7 — replace `_ => {}` in `prefix_message` with explicit arms | ~10 | `error.rs:386` | UNCHANGED |
| 10 | r6 §10#10 — stamp `retryable: true` wire flag on `OpError::coded` | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 11 | r7 §10#4 — SDK `withRetry` predicate add 3 retryable codes | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 12 | r6 §10#11 — route `migrations::coded()` through `DbError::Coded` or delete | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

---

## 6. Score

**92.5 / 100** (+1.5 vs r9's 91)

**What earned the +1.5 this cycle:**

- **`403b3891` (I12 ASCII allowlist on field names)** — closes a
  long-standing SDK-visible aliasing class. The new error message
  follows the inline-alphabet pattern (gold standard); the code is
  stable; the tests pin both directions. The new site is genuinely
  better than the older `audit.rs:818` twin still in carry. Real
  **+0.5.**
- **`51c342e8` (I6 release_advisory_lock returns Result)** — closes
  a silent-swallow site that had been carrying for the lifetime of
  the trait. The two production callers now warn with distinguishable
  caller-context substrings + uniform structured fields. Sets the
  precedent for the *new* caller-context-in-message-body pattern.
  Real **+0.25.**
- **`fcf7ce3c` (F1 warn-half, 5 sites)** — closes the long-running
  F1 observability gap on the warn side (the sweeper is still
  pending, per the commit message). All 5 sites satisfy the
  operator-help checklist (app + audit row + transition + retry
  context). The `apply.rs:Failed` site introduces the
  primary+secondary-error-in-one-warn shape. Real **+0.75.**

**What held the cycle back (no offsets, just unspent slack):**

- §6 `finalise_backfill` warn missing `name` + `collection` — **6th
  cycle in carry**. The new `release_advisory_lock` warn 12 lines
  below in the same function carries `name`; the contrast is sharper
  than ever. 2-LOC fix.
- r9 §3 `not_configured` overload — 2nd cycle, no movement.
- r9 §4 zero-callered constructors — 2nd cycle, no movement.
- r9 §6 #4 `audit.rs:818` alphabet — 3rd cycle, now has TWO in-tree
  exemplars (replication's twin + this cycle's new field-name site).
- r10 §8 no-op-prefix pin missing two recently-unified codes — 1st
  cycle in carry, but it's a 2-LOC ratchet.

**Net cycle: +1.5 (all three commits scored cleanly; nothing
regressed).**

**Why not higher (the -7.5 deficit, refreshed):**

- r9 §3 disambiguate `not_configured` — **+0.5–1.0** when split.
- r9 §2.LOW `validation_hinted` adoption (5 P0001 codes) — **+1.0.**
- r9 §5 `finalise_backfill` 2-LOC warn fix — **+0.5** observability.
- r9 §4 constructor zero-caller adoption-or-delete — **+0.25.**
- r9 §6 #4 `audit.rs:818` alphabet — **+0.25.**
- r10 §8 no-op-prefix pin ratchet — **+0.25.**
- r7 §7 `prefix_message` wildcard arm — **+0.5** defensive.
- Cross-crate SDK gaps (`toJSON` hint drop + `withRetry` predicate
  narrowness) — block ceiling to ~96. Out of native scope.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87) → r7 (91) → r8 (91)
→ r9 (91) → r10 (92.5).

**Plateau broken** — the rail moves +1.5 on the back of three
genuinely good commits this cycle. r9 forward-projected
"93-94 / 100" if §3 + §2.LOW + §5 all landed; none of those three
landed this cycle (each is still in carry), but three OTHER good
landings (I12, I6, F1 warn-half) yielded +1.5 anyway. The rail has
more low-hanging fruit than r9 acknowledged.

**Forward projection:** the r9 cycle-over-cycle prediction was too
narrow on which commits would move the score. If the next cycle
lands ANY two of: (a) §3 `not_configured` split, (b) §5
`finalise_backfill` 2-LOC, (c) §6 `audit.rs:818` alphabet, (d) §8
pin ratchet — the native rail should clear **93.5–94 / 100**. The
4 candidates are 11 LOC total; this is unforced free score the
maintainer can collect in a single 15-minute sitting.
