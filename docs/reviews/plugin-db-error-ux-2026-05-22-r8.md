# plugin-db Error-UX Review — 2026-05-22 r8

Scope: `crates/plugin-db/src/` at HEAD (post `f1c5184e`, `9e392ba1`,
`3d79d2da`, `09e32998`, `bc4363f0`). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r7.md` (91 / 100).

Lens: SDK-author error-handling discipline. Every finding evaluates
the JS-visible surface — `e.code`, `e.message`, `e.hint` — and whether
the SDK can branch on it without parsing strings. Plateau-acknowledged
cycle: deltas in the ±1 band are expected here. The native rail is
near its asymptote; remaining gains are SDK-side or small native
hygiene.

---

## TL;DR — what landed since r7

**Resolved:**

- **r7 §3.INFO — `DbError::Configuration` gains `hint` field** (commit
  `f1c5184e`). The variant most aligned with operator-side remediation
  now carries `hint: Option<String>` in the wire shape; the
  `to_op_error()` arm at `error.rs:264-266` forwards it verbatim to
  `OpError::coded`. Two of the six existing call sites now ship
  operator-useful hints (`wal_level_not_logical` and
  `not_provisioned`); the other four (`lazy_init_failed`,
  `backend_not_initialized`, `cic_configuration`, `not_configured`)
  stay `None` — three of those are invariant breaches where the SDK
  cannot help. **Closes r7 §3.INFO** and **r7 §10 rank 8**.
- **r7 §6 / MED — `replication.rs:213,257` substring matching against
  SQLSTATE codes** (commit `f6043126`). Both sites now read
  `e.as_db_error()?.code()` against `SqlState::DUPLICATE_OBJECT` and
  `SqlState::OBJECT_NOT_IN_PREREQUISITE_STATE` respectively
  (`replication.rs:216-221`, `:268-274`). The `wal_level` lowercase
  fallback is gone. Same shape as the `a272d1af` DETAIL-token sweep;
  the rail's "discriminate on machine ids, not formatted strings"
  rule now applies cleanly across `auth/session.rs` AND
  `replication.rs`. **Closes r7 §6/MED**.
- **`f6043126` companion** — `classify_p0001_detail` extracted into a
  pure `classify_detail_token(&str)` map; 7 new tests pin all 5
  DETAIL → (code, message) branches + the unknown-token fall-through
  + the codes-are-distinct invariant. Defensive coverage for the
  MAJOR-R5-1 closure shape.
- **`9e392ba1` — error.rs preamble rewritten** to accurately enumerate
  5 categories of `Result<_, String>` hold-outs (was: "saturated at 2
  sites"; actual: 8 sites across 5 categories). Including the
  cold-init category that exposes the §3 drift below. This is the
  honest documentation that lets the drift be spotted.

**Still open from r7 (carries):**

- **r7 §2.LOW — 5 P0001 `session_*` codes still hint-less.**
  `validation_hinted()` remains zero-callered. Unchanged.
- r7 §5 / r6 §5 — `finalise_backfill` warn missing `name` +
  `collection` fields.
- r7 §7 / r6 §6 — `prefix_message` wildcard `_ => {}` arm bypasses
  `#[non_exhaustive]` future variants.
- r6 §10#6 — `v8_classes/migration*.rs` `parse_*` rejects remain raw
  `TypeError` with no `.code` (intentional per `error.rs` preamble
  category 3 — TypeError-class, never need `.code`; documented
  exclusion, not a gap).
- Cross-crate SDK gaps: `toJSON` drops `hint` (verified at
  `sdks/db/src/collection.ts:46-62` — `name`, `message`, `code`,
  `errors` are picked up; `hint` is NOT); `withRetry` default
  predicate only matches `optimistic_lock_failure` (verified at
  `sdks/db/src/with-retry.ts:25-30`).

**New findings in r8:**

- **[MED-NEW] `not_configured` overloads 4 semantically distinct
  conditions across 6 production sites.** A single SDK `.code` no
  longer means a single thing.
- **[MED-NEW / promoted from r7 INFO] Code-name drift between
  `lazy_init_failed` and `not_configured` for the SAME underlying
  `init_pool_async` cold-init failure** persists. Same call (line 117
  in `register_model/mod.rs` vs `exec.rs:62` and `:315`) → different
  SDK codes.
- **[LOW] Two of the four hint-less Configuration codes
  (`lazy_init_failed`, `cic_configuration`) wrap underlying error text
  in the message body** — an operator could be told to "check the
  underlying error" but the variant has a slot for that now.
- **[INFO] `f1c5184e` adds `config_hinted()` but not `validation_hinted()`
  callers** — the hint-discipline split is now visibly lopsided: 2 of
  6 Configuration sites carry hints; 0 of 5 session-validation sites
  do. Same observation as r7 §2.LOW, sharpened.

**Net score delta vs r7:** see §10.

---

## 1. Verify r7 closures end-to-end

### [PASS] r7 §3.INFO — Configuration carries hint

Verified at `error.rs:144-148`:

```rust
Configuration {
    code: &'static str,
    message: String,
    hint: Option<String>,
}
```

`to_op_error()` arm at `error.rs:264-266` forwards `hint` verbatim to
`OpError::coded(code, message, hint)`. Six production literal sites
all carry the field:

| Site | Code | Hint? | Useful? |
|---|---|---|---|
| `wal_consumer.rs:349-358` | `not_provisioned` | YES | YES — names the env-var + invariant |
| `replication.rs:277-286` | `wal_level_not_logical` | YES | YES — "set wal_level=logical in postgresql.conf and restart" |
| `register_model/mod.rs:119-123` | `lazy_init_failed` | NO | n/a — invariant breach |
| `register_model/mod.rs:126-130` | `backend_not_initialized` | NO | n/a — invariant breach |
| `backend/postgres.rs:593-600` | `cic_configuration` | NO | n/a — retry-budget exhaustion, invariant-class |
| `exec.rs:64, :68, :317, :321` (via `config()`) | `not_configured` | NO | mixed (see §3) |

The wire format is now `OpError::coded(code, message, hint)` for
every Configuration. `DbError::config_hinted()` helper at
`error.rs:311-321` exists for the hint-bearing path. Solid landing.

### [PASS] r7 §6/MED — replication.rs substring → SQLSTATE typed

Verified at `replication.rs:216-221, :268-274`. Both sites now use
`e.as_db_error()?.code() == &SqlState::<NAME>` patterns. The
`msg.to_lowercase().contains("wal_level")` fallback is gone; the
typed code is the sole discriminator. The comments at both sites
explicitly reference the `auth/session.rs::classify_p0001_detail`
shape as the rule (lines 211-214, 264-267). The rail is now
internally consistent: zero remaining "SQLSTATE in rendered message
substring" sites in production.

Verification:
```
grep -nE 'msg\.contains\("[0-9]{5}|msg\.contains\("[0-9]{2}[A-Z]' \
    crates/plugin-db/src/
# 0 production hits
grep -n 'as_db_error()' crates/plugin-db/src/
# 3 production sites: auth/session.rs:177, replication.rs:217, :269
```

### [PASS] r5 closures still clean

All 4 r5 MAJORs remain closed (verified: zero hits for
`ConsumerError::NotProvisioned`, zero `msg.contains("P0001")`-style
patterns, `WalConsumer::new` signature still
`Result<Self, DbError>`).

---

## 2. Configuration hint discipline — are the 2 hints operator-useful?

Two hints shipped in `f1c5184e`. Both pass the "is this remediation,
not just rephrased message body?" sniff test.

### [PASS] `not_provisioned` hint (wal_consumer.rs:352-356)

```rust
message: "wal consumer: db_url not configured".to_string(),
hint: Some(
    "replication requires a connected runtime context — set \
     DATABASE_URL or pass --db-url so the runtime can mint a \
     replication=database connection"
        .to_string(),
),
```

The hint names two concrete remediations the operator can act on
(`DATABASE_URL` env var, `--db-url` CLI flag) AND mentions the
required connection mode (`replication=database`) which the message
body does NOT. This is genuine remediation, not paraphrase. PASS.

### [PASS] `wal_level_not_logical` hint (replication.rs:282-285)

```rust
message: format!(
    "replication: server is not configured for logical decoding \
     (underlying: {msg})"
),
hint: Some(
    "set wal_level=logical in postgresql.conf and restart"
        .to_string(),
),
```

The message states the condition; the hint states the action
verbatim (the exact `postgresql.conf` line + the restart
requirement). Both pieces are independently useful: an operator UI
can render the message in red and the hint in green. PASS.

### [LOW-NEW] Two hint-less Configurations have a clear use for `hint`

- **`lazy_init_failed`** (`register_model/mod.rs:119-123`): the
  message is `"db: lazy init failed: {e}"`. The `{e}` is whatever
  string `init_pool_async()` returned (containing the source-chain
  walk: ECONNREFUSED, TLS handshake failure, auth rejection, …). A
  hint like `"check DATABASE_URL credentials/host; the runtime cold-
  inits the pool on first use — most failures here are network or
  auth"` would help an operator triage. Today the hint slot is
  `None`.
- **`cic_configuration`** (`backend/postgres.rs:593-600`): message is
  `"db: create index '{name}' exhausted retry budget without a
  terminal result"`. This IS an invariant breach (the loop should
  have either returned success or a typed error before exhausting),
  but an operator-facing hint like `"this is an internal invariant
  breach — the create-index loop should always return a terminal
  state; please file a bug with the audit row"` would actually be
  the useful artefact. Today: `None`.

Severity: LOW — pure prose addition; no wire-shape change.
Decision call.

---

## 3. Code-name drift — `init_pool_async` cold-init splits 2 ways (verified)

### [MED-NEW] Same failure → 2 codes; promoted from r7 INFO

`init_pool_async` is called from three production sites; each
synthesises a Configuration error on the `Err` arm, but with
**different `.code` strings**:

| Caller | `.code` | Site |
|---|---|---|
| `orchestrator/register_model/mod.rs:117` | `lazy_init_failed` | DDL deploy first use |
| `exec.rs:62` | `not_configured` | per-query cold path |
| `exec.rs:315` (via `ensure_pool`) | `not_configured` | per-tx cold path |

All three call the same `init_pool_async()`. All three handle the
same underlying error (Postgres connect failure, TLS handshake,
auth). An SDK author writing a catch handler:

```ts
catch (e) {
  if (e.code === "lazy_init_failed") /* … */;
  // misses the other two paths
  if (e.code === "not_configured")   /* … */;
  // misses the register-model path AND collides with 3 other meanings
}
```

The error.rs preamble (`error.rs:30-34`) flags this honestly as a
documented follow-up. r7 §10#3 promoted "substring-match" to MED; the
analogous "same cause, different codes" pattern lives here.

**Fix sketch (~10 LOC)**: choose one code (`cold_init_failed` reads
better than either current option; or just `not_configured` for both)
and update both sites. Or introduce a single helper in `error.rs`:

```rust
pub(crate) fn cold_init_failed(e: String) -> DbError {
    DbError::config_hinted(
        "cold_init_failed",  // one code
        format!("db: lazy init failed: {e}"),
        "check DATABASE_URL credentials/host; pool is cold-init on first use",
    )
}
```

Then both call sites: `.map_err(crate::error::cold_init_failed)`.
Closes the drift + lands the LOW-NEW hint from §2 at the same time.

Severity: MED — same SDK observability fragility class as the
substring-match anti-pattern. The fix is mechanical, ~10 LOC, and
its absence makes the documented `error.rs:30-34` comment more
embarrassing the longer it sits.

### [MED-NEW] `not_configured` overloads 4 conditions

Grep returns 7 production literal sites for `"not_configured"`:

| Site | Underlying condition |
|---|---|
| `exec.rs:64,:317` | `init_pool_async` cold-init failure |
| `exec.rs:68,:321` | Pool not initialised AFTER `init_pool_async` claimed success (invariant breach) |
| `orchestrator/transaction.rs:147` | `db_url` not set at all in context |
| `orchestrator/auto_tx.rs:199` | Same as above |
| `v8_classes/migration.rs:269` | Backend not initialised (different slot from pool) |
| `v8_classes/migrations.rs:179-181` | Same as `migration.rs:269` |

Four distinct causes:

1. Cold-init failed (transient + remediable: check credentials)
2. Pool initialisation succeeded but pool slot is empty (invariant
   breach: should never happen)
3. `db_url` literally unset in the per-isolate context (operator
   config: plugin not registered, or `DB_URL` env missing at
   startup)
4. Backend trait object not minted (different context slot from pool;
   `init_pool_async` was never run on this thread, or the slot was
   cleared by `set_db_url` race)

A SDK author cannot tell these apart from `.code` alone. Message
substring matching (the rail's documented anti-pattern) is the only
recovery.

**Fix sketch**: split into:
- `not_configured` — case 3 only (operator: `DATABASE_URL` unset)
- `cold_init_failed` — case 1 (subsumes both `lazy_init_failed`
  callers from §3 above)
- `pool_not_initialised` — case 2 (invariant; debug_assert-equivalent)
- `backend_not_initialised` — case 4 (already exists at
  `register_model/mod.rs:127` as `backend_not_initialized`! Note the
  spelling drift too — `_initialized` vs `_initialised`. See §6.)

Severity: MED — same shape as the §3 drift but wider blast radius.

---

## 4. `validation_hinted` usage — re-verify zero callers

### [INFO] Zero production callers (unchanged from r7)

```
grep -rn 'validation_hinted' crates/plugin-db/src/
# 1 hit — declaration only at error.rs:333
```

The 5 P0001 `session_*` codes from `a272d1af` remain `validation()`
callers, not `validation_hinted()`. r5 §3, r6 §10#2, r7 §2.LOW — same
finding, third cycle in carry. The function exists; its first
production caller will take ~25 LOC across `auth/session.rs:194-214`.

Symmetry note: `f1c5184e` shipped `config_hinted()` AND landed 2
production callers in the same commit. The hint-discipline ratio is
now visibly lopsided:

- Configuration: 2 of 6 sites carry hints (33%)
- ValidationFailed: 0 of all callers carry hints (0%)
- Retryable variants: 3 of 3 carry hints (100%, but they're
  hard-coded in `to_op_error()`, not at the call site)

The rail principle stated in r7 §2.LOW — "the native side stamps the
wire facts; the SDK doesn't re-derive them" — applies just as much
to `session_*` codes as it does to `not_provisioned`.

---

## 5. Fresh error-site sample (8 sites)

| # | Site | Variant | `.code` | Hint? | Message clarity | Verdict |
|---|---|---|---|---|---|---|
| 1 | `auth/session.rs:259` (5 detail tokens) | `ValidationFailed` | 5 stable codes | NO | machine-token mapped to human one-liner | clear ✓; hint-less ✗ |
| 2 | `replication.rs:84-88` (empty app_id) | `ValidationFailed` | `invalid_app_id` | NO | "replication: app_id must not be empty" — fine | clear ✓ |
| 3 | `replication.rs:91-97` (bad char in app_id) | `ValidationFailed` | `invalid_app_id` | NO | names the char + alphabet — excellent | clear ✓ |
| 4 | `audit.rs:807-810` (empty app_id) | `ValidationFailed` | `invalid_app_id` | NO | "audit: app_id cannot be empty" — fine | clear ✓ |
| 5 | `audit.rs:816-819` (bad char) | `ValidationFailed` | `invalid_app_id` | NO | "audit: invalid app_id: {name}" — doesn't name the alphabet | could improve ⚠ |
| 6 | `orchestrator/transaction.rs:129-134` (bad isolation) | `ValidationFailed` | `invalid_isolation_level` | NO | enumerates the 4 valid values inline | excellent ✓ — exemplar |
| 7 | `orchestrator/transaction.rs:118-121` (nested tx) | `ValidationFailed` | `tx_already_active` | NO | "nested transactions not supported" — informative | clear ✓ |
| 8 | `v8_classes/transaction.rs:178-182` (settled) | `ValidationFailed` | `tx_settled` | NO | "transaction already committed or rolled back" — clear | clear ✓ |

**Sample observations:**

- **Message clarity is uniformly good.** No "internal error" leakage,
  no SQLSTATE strings in user-facing text, no Rust type names. Every
  message names the subsystem (`auth/session:`, `replication:`,
  `audit:`, `db:`) and the operation that refused.
- **All 8 codes are stable static strings.** Zero `format!()` codes,
  zero codes derived from input.
- **Site #6 is the exemplar pattern.** Inlining the valid values
  (`"Must be one of: read uncommitted, read committed, repeatable
  read, serializable"`) is exactly what the SDK can surface to a
  developer building a query. Site #5 (audit's `invalid app_id`)
  *should adopt the same pattern* — name the alphabet
  (`[A-Za-z0-9_-]`) inline as `replication.rs:93-96` already does.
  ~1 LOC fix.
- **Zero hint adoption** in the validation-class sample. Site #1 is
  the r7 §2.LOW carry; site #6 could carry a hint pointing at the
  Postgres docs URL; sites #2-#5, #7, #8 are debatable (the message
  IS the remediation for these). Skewed but consistent.

**Verdict**: messages are operator-/developer-clear across the
sample. The remaining gap is structured hint adoption.

---

## 6. Code-name spelling drift sanity check

### [LOW-NEW] `backend_not_initialized` (Z) vs `not_initialised` (S)

While auditing the §3 overload, noticed:

- `orchestrator/register_model/mod.rs:127` ships `.code =
  "backend_not_initialized"` (US spelling).
- `v8_classes/migration.rs:269` and `v8_classes/migrations.rs:181`
  ship `.code = "not_configured"` with message `"db: backend not
  initialised"` (UK spelling in the message body).

Two different codes for the same underlying condition (`backend()
returns None`) — partially the §3 overload, partially a spelling
drift in the prose. The rest of the codebase consistently uses UK
spelling in messages (`replication: probe pg_publication:`,
`auth/session:` is neutral, `db: backend not initialised`).

The `.code` strings are SDK-contract and don't have to match
spelling-style with messages, but consistency between the two
backend-not-init sites (one `backend_not_initialized`, one
`not_configured`) should be picked.

Severity: LOW — captures the §3 drift from a different angle.

---

## 7. Retry semantics — re-verify alignment

| `.code` | Variant | Auto-retry? | Hint? | r8 verdict |
|---|---|---|---|---|
| `transient` | `Transient` | YES | YES | ✓ unchanged from r7 |
| `serialization_failure` | `Serialization` | YES | YES | ✓ unchanged |
| `lock_not_available` | `LockContention` | YES | YES | ✓ unchanged |
| `unique_violation` | `UniqueViolation` | NO | NO | ✓ unchanged |
| `validation_refused` | `SchemaRefused` | NO | NO (envelope IS the message) | ✓ correct — SDK parses envelope |
| `not_provisioned` | `Configuration` | NO | YES (now ✓) | **promoted** — hint slot used post-`f1c5184e` |
| `wal_level_not_logical` | `Configuration` | NO | YES (now ✓) | **promoted** — hint slot used post-`f1c5184e` |
| `lazy_init_failed` | `Configuration` | NO | NO | ✓ semantically, but see §2 LOW (hint candidate) |
| `cic_configuration` | `Configuration` | NO | NO | ✓ semantically, but see §2 LOW |

Retryability remains correctly aligned. The post-`f1c5184e` change
materially improves the Configuration class — the rail can now say
"Configuration errors are not auto-retriable but DO carry operator
remediation in `.hint`" as a contract, not a wish.

Pinned by `retryable_variants_carry_hint` test (`error.rs:646-662`).

### [PASS] Cross-crate `withRetry` predicate still narrow

Re-verified at `sdks/db/src/with-retry.ts:25-30` — default predicate
matches `optimistic_lock_failure` only. None of the native rail's 3
retryable codes (`transient`, `serialization_failure`,
`lock_not_available`) are picked up by the default. Documented
escape hatch (`isOptimisticLockError(e) || (e as
{code?:string}).code === "serialization_failure"`) at line 9-12 is
correct but still requires the SDK author to know the codes. Same
finding as r6 §8 / r7 §8 cross-crate carry.

### [LOW-CARRY] `toJSON` drops `hint`

Re-verified at `sdks/db/src/collection.ts:46-65`. `toJSON` picks up
`name`, `message`, `code`, `errors`. Does NOT pick up `hint` —
`f1c5184e` added two new hint-bearing codes in this round, sharpening
the drop. SDK rejection chain currently throws away the operator
remediation prose before it reaches RPC consumers.

```ts
// at sdks/db/src/collection.ts:47-62 (verified)
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
    // hint NEVER added — even when the native error carries one
    return obj;
  },
  ...
});
```

3-LOC fix in SDK; cross-crate, tracked.

---

## 8. Other observations

### [INFO] `prefix_message` wildcard arm — unchanged (r6 §6, r7 §7 carry)

`error.rs:386` still uses `_ => {}` for structured-variant arms.
`DbError` is `#[non_exhaustive]` — a future SQLSTATE-derived variant
added without thinking about prefix semantics would silently bypass.
Tripwire-only.

### [INFO] `auth/bootstrap.rs:1083-1090` test enumerates the codes that should NEVER be prefixed

```rust
"wal_level_not_logical",
"not_configured",
// ValidationFailed codes never get re-prefixed.
"session_signature_expired",
...
```

This is an in-tree pin for the rail invariant — a structural test
that the structured variants resist prefix. Solid defensive coverage
(not a finding, just an acknowledgment).

### [PASS] Preamble re-write honest about hold-outs

`error.rs:9-42` now enumerates 5 categories:
1. Wire-contract envelopes (SchemaRefused)
2. Pure parsers (`auth/session.rs` hex decoders)
3. JS-input arg parsers (`v8_classes/migration*.rs::parse_*`)
4. Cold-init (the §3 drift, explicitly flagged)
5. Test helpers

This is the correct shape. r7 had stale "saturated at 2 sites" prose
that hid the cold-init drift; `9e392ba1` makes the drift visible. The
documentation now matches the code; r8 can score the drift against
the documented-but-unfixed state.

---

## 9. Concrete fixes ranked by SDK-author impact

| Rank | Finding | LOC | Files | Status vs r7 |
|---|---|---|---|---|
| 1 | r7 §10#1 — SDK `toJSON` drops `hint` (worsened by `f1c5184e` adding hints) | 3 | `sdks/db/src/collection.ts:46-65` | UNCHANGED (sharper) |
| 2 | r7 §2.LOW — adopt `validation_hinted()` for 5 `session_*` codes | ~25 | `auth/session.rs:194-214` | UNCHANGED (3rd carry) |
| 3 | **§3 NEW — unify `lazy_init_failed`/`not_configured` cold-init code** | ~10 | `register_model/mod.rs:119`, `exec.rs:64,:317` | NEW (MED) |
| 4 | **§3 NEW — split `not_configured` overload across 4 distinct conditions** | ~20 | `exec.rs`, `transaction.rs`, `auto_tx.rs`, `v8_classes/migration*.rs` | NEW (MED) |
| 5 | r7 §10#4 — SDK `withRetry` predicate add native rail's 3 retryable codes | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 6 | r7 §5 / r6 §5 — `finalise_backfill` warn: add `migration`, `collection` | 2 | `migrations.rs:642` | UNCHANGED |
| 7 | §2 LOW — add hints to `lazy_init_failed` + `cic_configuration` (free-ride on rank 3 fix) | ~6 | `register_model/mod.rs`, `backend/postgres.rs` | NEW (LOW) |
| 8 | r6 §6 — replace `_ => {}` in `prefix_message` with explicit structured arms | ~10 | `error.rs:386` | UNCHANGED |
| 9 | §5 #5 — name alphabet inline in `audit.rs:818` `invalid_app_id` message | 1 | `audit.rs:818` | NEW (LOW polish) |
| 10 | r6 §10#8 — augment `migrations.rs:329` "returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs` | UNCHANGED |
| 11 | r6 §10#10 — stamp `retryable: true` wire flag on `OpError::coded` | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 12 | r6 §10#11 — route `migrations::coded()` through `DbError::Coded` or delete | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

Ranks 3 + 4 are the natural pair: unifying the cold-init code (rank
3) buys 1 of the 4 split conditions in rank 4 for free.

---

## 10. Score

**91 / 100** (±0 vs r7's 91)

**What earned the +1.5 this cycle:**

- **`f1c5184e` (`Configuration` gains `hint`)** — closes r7 §3.INFO
  cleanly. Two operator-useful hints landed; the variant most needing
  remediation prose can now carry it without overloading the message
  body. The wire shape is now consistent across all four hint-bearing
  classes (Configuration, retryable Serialization/LockContention/
  Transient). Real **+1.0**.
- **`f6043126` (SQLSTATE-typed checks in replication.rs)** — closes
  r7 §6/MED. The rail's "discriminate on machine ids, not strings"
  rule is now applied uniformly across `auth/session.rs` AND
  `replication.rs`. Companion: 7 new `classify_detail_token` unit
  tests pin the MAJOR-R5-1 contract. Real **+0.5**.
- **`9e392ba1` (preamble accuracy)** — documentation hygiene; no
  behaviour change. Real **+0** but it un-hides the §3 drift the
  prior cycle's prose hid. Truthful documentation enables this
  cycle's audit to find drift. Negative-marker prevention; tracked
  but unscored.

**What earned the -1.5 this cycle (offsetting):**

- **§3 NEW / MED — `not_configured` overloads 4 conditions** —
  documented as a follow-up in the preamble (`9e392ba1`) but still
  shipping. The same SDK fragility shape as substring-matching:
  "same code, different meaning" is just as bad as "wrong-classified
  string". Real **-1.0**.
- **§3 NEW / MED — `lazy_init_failed` ≠ `not_configured` for the
  same cold-init call** — promoted from r7 INFO. Same call site
  forks into two codes; an SDK author who handles one misses the
  other. Real **-0.5**.

**Net cycle: +1.5 - 1.5 = 0.** Score holds at 91.

**Why not higher (the -9 deficit, refreshed):**

- §3 drift (~MED) — same fragility class as the closed substring
  match; **+1.5** when unified (rank 3 + 4 land together).
- §2.LOW carry (3rd cycle) — 5 P0001 codes still hint-less; **+1.0**
  when `validation_hinted` adoption lands.
- §5 #5 audit invalid_app_id alphabet — **+0.25** polish.
- §6 finalise_backfill name/collection — **+0.5** observability.
- §7 prefix_message wildcard — **+0.5** defensive.
- Cross-crate SDK gaps (`toJSON` hint drop + `withRetry` predicate
  narrowness) — block ceiling to ~96. Sharpened by `f1c5184e` (hints
  are now being dropped on the wire). Out of scope but tracked.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87) → r7 (91) → r8 (91).

The plateau is real and expected (cycle 09:00 plateau signal). The
native rail's structural work is **done**: the 4 r5 MAJORs all
closed; the substring-match anti-pattern fully eliminated; the
Configuration hint slot is now first-class. What remains is
discipline (drift, naming consistency, hint adoption across
ValidationFailed) — small individually, additive in aggregate.

If §3 (drift unification, ~30 LOC) + §2.LOW (`validation_hinted`
adoption, ~25 LOC) land next cycle, the native rail should reach
**93-94 / 100**. Anything higher requires the SDK-side
`toJSON`/`withRetry` motion or a substantively new wire feature
(e.g., `retryable: bool` flag on `OpError`).

The plateau signal from cycle 09:00 is correctly calibrated: this
cycle moved two specific gaps (Configuration hint slot, replication
substring match) but uncovered an equal-weight gap (cold-init code
drift) that was previously hidden by stale preamble prose. Net zero
is the honest answer.
