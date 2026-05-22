# plugin-db Error-UX Review — 2026-05-22 r7

Scope: `crates/plugin-db/src/` at HEAD (post `a272d1af`, `aa639715`,
`e5315083`, `386f9bf5`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r6.md` (87 / 100).

Lens: SDK-author error-handling discipline. Every finding evaluates the
JS-visible surface — `e.code`, `e.message`, `e.hint` — and whether the
SDK can branch on it without parsing strings. Operator observability
(tracing fields) is in scope where it overlaps with the error-UX rail.

`cargo test -p zeroship-plugin-db --lib` at HEAD: **364 passed**.

---

## TL;DR — what landed since r6

**Resolved (the two highest-value open items from r6 closed):**

- **r6 §2 / Pilot Pick #1 — `auth/session.rs` P0001 substring matching**
  closed by `a272d1af`. The SECURITY DEFINER `init_session` now sets a
  `DETAIL` token on every `RAISE EXCEPTION USING ERRCODE = 'P0001'`
  (`bootstrap.rs:521-557`); the Rust side reads
  `e.as_db_error()?.detail()` via a new `classify_p0001_detail()` helper
  (`session.rs:174-202`) and maps each of **5** detail tokens to a
  stable `.code`. Locale- and formatter-independent. **Closes MAJOR-R5-1.**
- **r6 §3 / Pilot Pick #2 — `WalConsumer::new` flattens `DbError` to
  `ConsumerError::NotProvisioned(String)`** closed by `aa639715`.
  `WalConsumer::new` now returns `Result<Self, DbError>`; the
  `ConsumerError::NotProvisioned(String)` variant is **deleted** (not
  just bypassed), so the type system enforces the typed path. Two
  failure classes propagate verbatim: `ValidationFailed {
  code: "invalid_app_id" }` and `Configuration { code: "not_provisioned" }`.
  Dispatch site at `replication_ops.rs:245-253` forwards
  `e.to_op_error()` with no re-stamp. **Closes MAJOR-R5-4.**
- **`e5315083`** — dead `format!("{e}")` + source-chain walk in
  `init_session`'s `map_err` deleted (post-DETAIL classification, the
  message build was wasted work).  `mark_consumer_running` gated to
  `cfg(test)` so the non-atomic footgun no longer ships in production.
  Module preamble for `replication_ops` now documents both legs of
  `WalConsumer::new`'s failure surface.
- **`386f9bf5`** — adds 5 lifecycle tests for the new
  `ConsumerRunningGuard` *and caught a latent bug*: `won.then_some(Self
  { app_id })` evaluated `Self { app_id }` eagerly, so the loser of an
  atomic claim race constructed and immediately dropped a `Self`,
  firing the custom `Drop` that *unmarked the winner's claim*. Fixed
  via `won.then(|| Self { app_id })`. The atomicity of `70921112` was
  silently broken. The fix is correct; this is a textbook
  Drop-side-effect-during-temporary bug.

**Still open from r6 (unchanged) — net carry into r7:**

- r6 §5 — `finalise_backfill` warn missing `name` + `collection`
  fields (both in scope).
- r6 §6 — `prefix_message` wildcard `_ => {}` arm bypasses
  `#[non_exhaustive]` future variants.
- r6 §10#6 / r5 §10#5 — `v8_classes/migration*.rs` `parse_*` rejects
  remain raw `TypeError` with no `.code`.
- r6 §8 — SDK `withRetry` default predicate only matches
  `optimistic_lock_failure` (cross-crate, SDK).
- r6 §9 — SDK `toJSON` drops `hint` (cross-crate, SDK).
- **r5 §3 hint discipline survives r6 unchanged** — `a272d1af` chose
  `validation()`, not `validation_hinted()`, for all 5 P0001
  promotions. r6 §10#2 expected both to land together.

**New findings in r7:**

- **[MED] `replication.rs:213,257` — same substring-matching anti-pattern
  the `a272d1af` work just eliminated from `auth/session.rs`** lingers
  in two production sites: `msg.contains("42710")` and
  `msg.contains("55000") || msg.to_lowercase().contains("wal_level")`.
  The `compio_postgres::Error::as_db_error()?.code()` API is now
  established locally — these should switch to SQLSTATE constants for
  consistency with the rail.
- **[LOW] `DbError::Configuration` carries no `hint` field** — the
  variant that *most* needs an operator-facing remediation hint
  (`wal_level_not_logical`, `not_provisioned`, `not_configured`)
  cannot carry one. The hint text is buried in `message` instead.
- **[INFO] `validation_hinted()` still has zero production callers**
  (`error.rs:291`). The 5 new `session_*` codes from `a272d1af` are
  the natural first callers and would close r5 §3 trivially.

**Net score delta vs r6:** see §10.

---

## 1. Verify r6 closures end-to-end

### [PASS] r6 §2 — P0001 substring matching → DETAIL-token classification

Verified at HEAD:

- `auth/bootstrap.rs:521-557` — the SECURITY DEFINER `init_session`
  function sets `DETAIL = '<token>'` on every `RAISE EXCEPTION USING
  ERRCODE = 'P0001'`. Five tokens:
  `session_signature_expired`, `session_invalid_actor_kind`,
  `session_nonce_too_short`, `session_nonce_replay`,
  `session_invalid_signature`.
- `auth/session.rs:174-202` — `classify_p0001_detail()` reads
  `e.as_db_error()?.code()` (must be `SqlState::RAISE_EXCEPTION`),
  then `.detail()`, then matches the 5 tokens to
  `(static_code, operator_message)` pairs.
- `auth/session.rs:229-249` — `init_session`'s `map_err` calls the
  classifier; on `Some` returns `DbError::validation(code, msg)`; on
  `None` falls through to `coded_sql("init_session", e)` (generic
  SQLSTATE).

Verification:
```
grep -n 'DETAIL = ' crates/plugin-db/src/auth/bootstrap.rs
# 5 hits at 524, 530, 537, 549, 557 — one per RAISE
grep -n 'classify_p0001_detail' crates/plugin-db/src/auth/session.rs
# 2 hits: declaration at 174, call site at 245
grep -n 'msg.contains' crates/plugin-db/src/auth/session.rs
# 0 hits in production paths (test asserts at lines 957/978 are
# string-shape assertions on the returned message, not classification)
```

### [PASS] r6 §3 — `WalConsumer::new` typed Result with distinct codes

Verified at HEAD:

- `wal_consumer.rs:346-368` — `pub fn new(...) -> Result<Self, DbError>`.
- `ConsumerError::NotProvisioned(String)` variant **deleted** entirely
  (commit message confirms; grep returns zero hits).
- `wal_consumer.rs:347-354` — empty `db_url` → `DbError::Configuration
  { code: "not_provisioned" }`.
- `wal_consumer.rs:359-360` — `slot_name`/`publication_name` return
  `Result<_, DbError>`; `?` propagates `DbError::ValidationFailed {
  code: "invalid_app_id" }`.
- `replication_ops.rs:245-253` — dispatch forwards `e.to_op_error()`
  verbatim; the prior re-stamp to `Configuration { code:
  "not_provisioned" }` is gone.
- Two new SDK-contract tests:
  `wal_consumer_new_invalid_app_id_returns_typed_error` and
  `wal_consumer_new_missing_db_url_returns_configuration`.

Verification:
```
grep -n 'ConsumerError::NotProvisioned' crates/plugin-db/src/
# 0 hits — variant fully removed
grep -n 'fn new' crates/plugin-db/src/wal_consumer.rs
# Shows `Result<Self, DbError>` signature at line 346
```

SDK can now branch:
```ts
try { ... } catch (e) {
  if (e.code === "invalid_app_id") /* developer/deploy error */;
  else if (e.code === "not_provisioned") /* operator/config error */;
}
```

### [PASS] r6 closures verified clean — all 4 r5 MAJORs closed

| r5 MAJOR | Description | Closed by | r7 status |
|---|---|---|---|
| MAJOR-R5-1 | `auth/session.rs` P0001 substring | `a272d1af` | **CLOSED** |
| MAJOR-R5-2 | (already closed in r6) | — | CLOSED (carried) |
| MAJOR-R5-3 | (already closed in r6) | — | CLOSED (carried) |
| MAJOR-R5-4 | `WalConsumer::new` flatten | `aa639715` | **CLOSED** |

All r5 MAJORs now closed. The native rail is no longer carrying
substring-classification fragility.

---

## 2. P0001 DETAIL classification — SDK branchability

### [PASS] SDK can branch on `.code` for each of 5 session validation cases

The 5 session-validation P0001 paths all stamp distinct `.code` values
on `OpError`:

| RAISE (bootstrap.rs) | DETAIL | `.code` on `OpError` |
|---|---|---|
| `'session-init signature expired'` | `session_signature_expired` | `session_signature_expired` |
| `'invalid actor_kind: %'` | `session_invalid_actor_kind` | `session_invalid_actor_kind` |
| `'nonce too short (need >=16 bytes)'` | `session_nonce_too_short` | `session_nonce_too_short` |
| `'session-init nonce replay detected'` | `session_nonce_replay` | `session_nonce_replay` |
| `'invalid session-init signature'` | `session_invalid_signature` | `session_invalid_signature` |

All 5 route through `DbError::validation(code, op_msg)` →
`ValidationFailed { code, message, hint: None }` →
`OpError::coded(code, message, None)`. SDK pattern:

```ts
catch (e) {
  switch (e.code) {
    case "session_signature_expired":   /* re-mint */ break;
    case "session_invalid_actor_kind":  /* fix caller */ break;
    case "session_nonce_too_short":     /* fix caller */ break;
    case "session_nonce_replay":        /* fresh nonce */ break;
    case "session_invalid_signature":   /* refresh creds */ break;
  }
}
```

Each is locale- and formatter-independent: the DETAIL token is a
stable machine identifier, not a translated/formatted human message.
This is the **correct shape**; r5 §10#2 / r6 §2 closed cleanly.

### [LOW] `auth/session.rs:174-202` — no hint on any of the 5 codes

  Why: SDK-author impact — the 5 codes are correctly distinguishable
  but carry no `hint`. The user-facing message is the operator-side
  body (`"auth/session: nonce replay detected"`), which is not what
  the SDK wants to show the end user. r5 §3 flagged this; r6 §2
  expected `a272d1af` to land hints alongside; it did not. The
  helper `validation_hinted()` exists at `error.rs:291` with zero
  production callers — these 5 sites are the natural first
  consumers.

  Fix: replace `DbError::validation(code, msg)` with
  `DbError::validation_hinted(code, msg, "<remediation>")` for each
  case. The 5 hints write themselves:

  ```rust
  ("session_signature_expired", "auth/session: signature expired",
      "re-mint via mint_session_token (default TTL is 5 minutes)"),
  ("session_nonce_replay",      "auth/session: nonce replay detected",
      "the nonce was already used; mint a fresh token and retry"),
  ("session_invalid_signature", "auth/session: invalid signature",
      "token was minted against a retired key or tampered with; refresh credentials"),
  ("session_invalid_actor_kind", "auth/session: invalid actor_kind",
      "actor_kind must be one of: auto, user, operator, ai-builder, platform"),
  ("session_nonce_too_short", "auth/session: nonce too short (need >=16 bytes)",
      "pass at least 16 random bytes as the nonce"),
  ```

  Verification:
  ```
  grep -n 'DbError::validation(' crates/plugin-db/src/auth/session.rs
  # 5 hits in classify_p0001_detail's call site
  grep -rn 'validation_hinted' crates/plugin-db/src/
  # 1 declaration (error.rs:291), 0 production callers
  ```

  Severity: LOW — every wire `.code` is correct; the SDK can
  surface remediation text by hard-coding the 5 cases on its side.
  But the rail principle is "the native side stamps the wire facts;
  the SDK doesn't re-derive them". Adopting `validation_hinted`
  here is the single edit that makes that principle observable in
  production code.

---

## 3. WalConsumer::new — distinct codes verified

### [PASS] SDK sees `invalid_app_id` vs `not_provisioned`

Verified at `wal_consumer.rs:346-368` (above). The two failure
classes are structurally distinct in the type — there is no shared
parent variant to collapse them. The dispatch path forwards
`e.to_op_error()` verbatim. Tests pin both legs:

- `wal_consumer_new_invalid_app_id_returns_typed_error`
  asserts `code == "invalid_app_id"` AND message contains
  "invalid character" AND `hint.is_none()`.
- `wal_consumer_new_missing_db_url_returns_configuration`
  asserts `code == "not_provisioned"` AND message contains "db_url".

The third regression test (`wal_consumer_rejects_invalid_app_id`)
already exists and was updated to match the typed variant. Three
tests covering this surface: solid.

### [INFO] `DbError::Configuration` carries no `hint` field

  Why: the variant most aligned with operator-side remediation
  (`wal_level_not_logical`, `not_provisioned`, `not_configured`)
  cannot carry an `Option<String>` hint. Today the remediation text
  is folded into `message`:

  ```rust
  // wal_consumer.rs:348-353
  DbError::Configuration {
      code: "not_provisioned",
      message: "wal consumer: db_url not configured \
                (replication requires a connected runtime context)"
          .to_string(),
  }
  ```

  The SDK can read `.message` for context but cannot reliably
  separate "what went wrong" from "what to do about it". A `hint`
  field would let the operator-facing UI render structured
  remediation (e.g. a TUI sidebar) without parsing the message body.

  Fix (small surface change):
  ```rust
  DbError::Configuration {
      code: &'static str,
      message: String,
      hint: Option<String>,
  }
  // to_op_error: OpError::coded(code, message, hint)
  ```

  Six sites set `Configuration`; each can decide whether a hint is
  warranted. Migration is mechanical (add `hint: None` to the
  4-5 sites that already exist, set `Some(...)` on 1-2).

  Verification:
  ```
  grep -n 'DbError::Configuration' crates/plugin-db/src/
  # 6 production sites
  grep -n 'pub fn config' crates/plugin-db/src/error.rs
  # 1 helper, builds Configuration with no hint slot
  ```

  Severity: INFO — pure type-shape evolution; the wire format
  already supports `hint` (it's `Option<String>` on `OpError`).
  Decision call.

---

## 4. lock_guard tracing::warn shape

### [PASS] Unlock-SQL warn carries `key + tag + error`

Re-verified at `orchestrator/lock_guard.rs:168-179` (unchanged from r6):

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

- `key` — `"zs_reg:<app_id>"` namespace ✓
- `tag` — `"register_model"` stage ✓
- `error` — `DbError` body via `walk_pg_chain` ✓
- Message body names consequence + checklist ✓

Companion Drop log at `lock_guard.rs:227-237` carries `key` + `tag`
(no `error` — Drop has no `Result` to inspect). Both are operator-
correct.

### [PASS] Structural test pins [I42] await-order

`386f9bf5` adds
`release_flips_flag_after_unlock_await_structural` at
`lock_guard.rs:348-382` which `include_str!`s its own source and
asserts via byte-offset that `self.released = true` appears
**after** `.query_text_params(...).await`. A silent revert to the
pre-`bd1e7ce1` order trips at unit-test time. Mirrors the
mint_subscription structural-test pattern. Solid defensive coverage.

---

## 5. finalise_backfill tracing::warn shape

### [PASS] Carries `app_id + audit_id + terminal + error`

Re-verified at `migrations.rs:638-651` (unchanged since 51ced4a0):

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

All four named fields present. Message names consequence + diagnostic
guidance. Solid operator log.

### [LOW] Two in-scope fields still missing: `name` + `collection`

Same finding as r6 §5, unchanged. The `lock_snapshot()` destructure
at `migrations.rs:453` brings `(name, collection, audit_id, dry_run,
start_generation)` into scope — both `name` and `collection` are
free to add:

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

  Why: operator-debug impact — joining via `audit_id` alone forces
  one DB hop to learn which migration stalled. With `migration` and
  `collection` in the line, the warn is self-describing.

  Verification:
  ```
  grep -n 'lock_snapshot()' crates/plugin-db/src/migrations.rs
  # line 453 — destructures (name, collection, audit_id, ...)
  grep -n 'tracing::warn!' crates/plugin-db/src/migrations.rs
  # 1 hit at line 642
  ```

  Severity: LOW — observability polish. Carried from r6 §5.

---

## 6. `.code` discipline sweep

### [PASS] Zero remaining `OpResult::Failed` rejections in production

Verified via grep:

```
grep -n 'OpResult::Failed' crates/plugin-db/src/
# 3 hits — all in comments/docstrings (error.rs:251 docstring,
# orchestrator/auto_tx.rs:64 + :298 comments). Zero in production code.
```

### [PASS] Zero remaining `OpError::coded(_, &e.to_string(), _)` patterns

```
grep -E 'coded\([^,]+,\s*&?e\.to_string\(\)' crates/plugin-db/src/
# 0 hits
```

### [PASS] All `RejectError` paths route through typed `DbError` → `to_op_error()`

Sampled 24 call sites across 6 files; every one is either:
- `value: ResolveValue::RejectError(e.to_op_error())` (post-typed)
- `value: ResolveValue::RejectError(e)` where `e: OpError` was minted
  earlier via `coded()` (migrations) or a `DbError::config(...)` literal
- `value: ResolveValue::RejectError(DbError::from(e).to_op_error())`
  in `crud.rs:75` for builder errors

No stringly-typed paths remaining on the V8 boundary in production.

### [PASS] `coded_db` consolidated to single variant-walker

`migrations.rs:87-96` no longer open-codes the variant match — it
delegates to `crate::error::prefix_message` (closes a sub-r6 carry).

### [MED] `replication.rs:213,257` — substring matching against SQLSTATE codes (NEW)

This is the **same anti-pattern `a272d1af` eliminated from
`auth/session.rs`**, but it persists in two replication sites:

`replication.rs:211-218`:
```rust
if let Err(e) = pool.execute(&pub_sql, &[]).await {
    let msg = format!("{e:#}");
    if !msg.contains("42710") {
        let mut err = DbError::from_pg(&e);
        prefix_message(&mut err, "replication: CREATE PUBLICATION: ");
        return Err(err);
    }
}
```

`replication.rs:250-273`:
```rust
.map_err(|e| {
    // 55000 (object_not_in_prerequisite_state) is the
    // canonical SQLSTATE when wal_level != logical. Surface
    // it as a `Configuration` error so the operator sees the
    // fix (and the SDK does NOT retry it) instead of a
    // generic "server error".
    let msg = format!("{e:#}");
    if msg.contains("55000") || msg.to_lowercase().contains("wal_level") {
        DbError::Configuration {
            code: "wal_level_not_logical",
            message: format!(...)
        }
    } else { ... }
})?;
```

  Why: SDK-author impact — same fragility class as the closed
  `auth/session.rs` site. SQLSTATE `"42710"` (duplicate_object) and
  `"55000"` (object_not_in_prerequisite_state) are stable; a
  Postgres patch release won't change them. But the discriminator
  is `msg.contains("42710")` — relies on the SQLSTATE appearing
  verbatim in the formatted body (true today, but the formatter
  has changed shape in past compio-postgres releases). The
  `wal_level` fallback (`msg.to_lowercase().contains("wal_level")`)
  is the explicit admission that the substring is unreliable. The
  whole point of `a272d1af` was to replace this pattern with
  `e.as_db_error()?.code()`.

  Fix (sketch):
  ```rust
  use compio_postgres::error::SqlState;
  if let Err(e) = pool.execute(&pub_sql, &[]).await {
      let is_duplicate = e.as_db_error()
          .is_some_and(|db| db.code() == &SqlState::DUPLICATE_OBJECT);
      if !is_duplicate {
          let mut err = DbError::from_pg(&e);
          prefix_message(&mut err, "replication: CREATE PUBLICATION: ");
          return Err(err);
      }
  }

  // for line 257:
  let is_prereq = e.as_db_error()
      .is_some_and(|db| db.code() == &SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE);
  if is_prereq {
      DbError::Configuration { code: "wal_level_not_logical", ... }
  } else { ... }
  ```

  Verification:
  ```
  grep -n 'msg.contains' crates/plugin-db/src/replication.rs
  # 2 production hits at 213 and 257
  grep -n 'as_db_error' crates/plugin-db/src/
  # 1 hit (auth/session.rs:177) — established API, just not adopted here
  ```

  Severity: MED — the pattern is identical to MAJOR-R5-1 modulo
  the variant the result maps to. Promoting r5/r6 "auth/session
  substring" to a generalised rule and applying it consistently
  is the natural next motion.

  Note: there is a third pattern at `migrations.rs:329` (per r6
  §10#8) augmenting the "migration insert returned no row" message
  — that one is not a SQLSTATE substring match, but lives in the
  same hygiene bucket as the rest of this section.

---

## 7. Hint discipline

### [INFO] `validation_hinted` still has 0 production callers

`error.rs:291-301` declares the helper; `grep` finds zero call
sites. The 5 new `session_*` codes (§2.LOW above) are the obvious
first consumers. r5 §3, r6 §10#2 — unchanged.

### [PASS] New DbError variants carry hints where retryable

Retryable variants (Serialization, Transient, LockContention) all
carry hints via `to_op_error()` (`error.rs:225-239`). Pinned by
the `retryable_variants_carry_hint` test (`error.rs:604-619`).
Non-retryable variants carry no hint by contract — also pinned.

### [LOW] Configuration variants surface remediation in `message`, not `hint`

See §3 INFO above. `wal_level_not_logical`'s remediation
("set wal_level=logical in postgresql.conf and restart") is in
the message body, not in a structured `hint`. The SDK has no way
to render "what went wrong" separately from "how to fix it" for
Configuration errors.

---

## 8. Retry semantics — sample 5 codes

| `.code` | Variant | Retryable? | Hint? | Verdict |
|---|---|---|---|---|
| `transient` | `Transient` | YES | YES | ✓ correct — connection drop, retry connection |
| `serialization_failure` | `Serialization` | YES | YES | ✓ correct — Postgres SSI, retry txn |
| `lock_not_available` | `LockContention` | YES | YES | ✓ correct — brief contention, retry |
| `unique_violation` | `UniqueViolation` | NO | NO | ✓ correct — user-input refusal |
| `not_provisioned` | `Configuration` | NO | NO | ✓ correct — operator must set DB_URL |
| `invalid_app_id` | `ValidationFailed` | NO | NO | ✓ correct — deploy error |
| `session_nonce_replay` | `ValidationFailed` | NO | NO* | ✓ correct semantically — SHOULD HINT "mint fresh token" |
| `session_signature_expired` | `ValidationFailed` | NO | NO* | ✓ correct semantically — SHOULD HINT "re-mint within TTL" |
| `migration_already_running` | `Coded` | NO | YES | ✓ correct — another worker holds; not retryable client-side |

(*) — semantically correct (not auto-retryable), but the hint slot
is empty. The SDK can show the user "your token expired" but has
to hard-code "mint a fresh one" itself.

Retryability is correctly aligned with variant choice in every
sampled case. The only gap is hint-presence on the 5 `session_*`
codes (LOW; §2.LOW above).

### [PASS] Cross-crate retry rail: `withRetry` predicate still narrow

Re-verified at `sdks/db/src/with-retry.ts:25-30` — default predicate
only matches `optimistic_lock_failure`. None of the native rail's
retryable codes (`transient`, `serialization_failure`,
`lock_not_available`) are picked up. Same r6 §8 finding;
cross-crate, tracked in §10.

---

## 9. Other observations

### [INFO] `prefix_message` wildcard arm — unchanged from r6 §6

`error.rs:343` still uses `_ => {}` to skip the structured-variant
arms. `DbError` is `#[non_exhaustive]` (`error.rs:55`), so a new
SQLSTATE-derived variant added in the future would silently bypass
prefix. Tripwire-only; no current bug. r6 §6 carries unchanged.

### [INFO] `let _ = lock_guard.release().await` at 3 sites

Three callers swallow the outer `Result` from `release()`:
`register_model/apply.rs:226`, `bootstrap.rs:137`, `mod.rs:217`.
The inner warn fires from inside `release()` itself, so observability
is preserved — but the call shape is opaque to readers. r6 §7 carry,
unchanged.

### [PASS] No new latent bugs introduced by the four commits

`386f9bf5` *uncovered* one latent bug (`then_some` Drop-side-effect)
and shipped its fix in the same commit. The fix is correct:
`won.then(|| Self { app_id })` lazily evaluates the closure only on
the winning branch, so the loser never constructs a `Self` and thus
never fires the `Drop` that would unmark the winner's claim. Tests
added (`guard_drop_unmarks_app`, `guard_drop_unmarks_on_panic_unwind`,
`guard_try_claim_loses_when_already_marked`) pin the invariant.

---

## 10. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC | Files | Status vs r6 |
|---|---|---|---|---|
| 1 | r6 §9 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts` | UNCHANGED |
| 2 | §2.LOW — adopt `validation_hinted()` for the 5 `session_*` codes (closes r5 §3, r6 §10#2) | ~25 | `auth/session.rs:174-202` | NEW (Pilot Pick #1 partially landed) |
| 3 | §6 — switch `replication.rs:213,257` to `as_db_error()?.code()` SQLSTATE match (matches `a272d1af`) | ~20 | `replication.rs` | NEW |
| 4 | r6 §8 — SDK `withRetry` predicate add `transient`/`serialization_failure`/`lock_not_available` | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 5 | r6 §5 / §5 — `finalise_backfill` warn: add `migration = %name`, `collection = %collection` | 2 | `migrations.rs:642` | UNCHANGED |
| 6 | r6 §10#6 — `v8_classes/migration*.rs` `parse_*` → typed `DbError::ValidationFailed { code: "invalid_migration_spec" }` | ~15 | `v8_classes/migration.rs`, `v8_classes/migrations.rs` | UNCHANGED |
| 7 | r6 §6 — replace `_ => {}` in `prefix_message` with explicit structured arms (tripwire) | ~10 | `error.rs:343` | UNCHANGED |
| 8 | §3.INFO — add `hint: Option<String>` to `DbError::Configuration` | ~15 | `error.rs`, ~6 call sites | NEW |
| 9 | r6 §10#8 — augment `migrations.rs:329` "returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs` | UNCHANGED |
| 10 | r6 §10#10 — stamp `retryable: true` wire flag on `OpError::coded` | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 11 | r6 §10#11 — route `migrations::coded()` through `DbError::Coded` or delete it | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

The native rail is now ~done — items 1, 4 are SDK-side; items 2, 3, 5,
7, 8 are 5-25 LOC native edits.

---

## 11. Score

**91 / 100** (+4 vs r6's 87)

**What earned the +4:**

- **`a272d1af` (P0001 DETAIL classification)** — closes MAJOR-R5-1.
  The biggest open SDK-error-UX gap on the native side. SDK now
  branches on 5 stable codes, locale-independent. Real **+2**.
- **`aa639715` (WalConsumer::new typed Result)** — closes MAJOR-R5-4.
  The `ConsumerError::NotProvisioned(String)` variant is deleted
  (not just bypassed), so the type system enforces typed propagation.
  SDK sees distinct codes for developer-error vs operator-config.
  Real **+1.5**.
- **`386f9bf5` (latent Drop-side-effect bug fix + 5 lifecycle tests +
  structural [I42] test)** — caught a real concurrency bug that would
  have silently broken `ConsumerRunningGuard`'s atomicity. Three new
  lifecycle tests pin the contract. Real **+0.5**.
- **`e5315083` (dead-code cleanup)** — perf cleanup (deleted the dead
  `format!` allocation) + non-atomic `mark_consumer_running` gated to
  cfg(test) so it can't leak into production. Real **+0**.

**Why not higher (the -9 deficit):**

- **§2.LOW (5 P0001 codes still hint-less)** — the natural counterpart
  to `a272d1af`. Adopting `validation_hinted()` for these 5 sites is
  the single edit that closes r5 §3 and completes the P0001
  promotion. ~25 LOC. **+1.5 trivially** when it lands.
- **§6 / NEW (replication.rs:213,257 substring match)** — the same
  anti-pattern `a272d1af` just eliminated, persisting in 2 other
  sites. ~20 LOC. **+1** when it lands.
- **§5 / r6 §5 (finalise_backfill missing `name` + `collection`)** —
  2-line observability polish. **+0.5** when it lands.
- **§3.INFO / NEW (Configuration variant has no hint slot)** — small
  type-shape change with downstream value. **+0.5** when it lands.
- **§7 / r6 §6 (prefix_message wildcard tripwire)** — pure
  defensive. **+0.5**.
- **Cross-crate SDK gaps (`toJSON` + `withRetry`)** — block ceiling
  to ~96 until they land. Out of scope.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87) → r7 (91).

The two highest-value native MAJORs from r5/r6 closed in this
window. The remaining 9 points split:

- ~4 points concentrated in 4 native edits (§2.LOW, §6, §5, §7),
  each 2-25 LOC.
- ~5 points held by SDK-side rail (`toJSON`/`withRetry`).

If §2.LOW + §6 + §5 land next cycle, the score should reach
**94 / 100**, exactly as r6 forecast. The native rail then reaches
its asymptote — further gains require SDK-side motion.

The native error-UX rail is structurally **done** for the four r5
MAJORs and the three observability surfaces (lock_guard,
finalise_backfill, ConsumerRunningGuard); what remains is hint
discipline and a small substring-match cleanup that follows the
pattern `a272d1af` established.
