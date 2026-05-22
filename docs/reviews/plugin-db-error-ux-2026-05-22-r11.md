# plugin-db Error-UX Review — 2026-05-22 r11

Scope: `crates/plugin-db/src/` at HEAD `6ff294fd` (cycles 12:47 + 13:17
+ 13:47 + 14:17 landed). Re-audit relative to
`docs/reviews/plugin-db-error-ux-2026-05-22-r10.md` (92.5 / 100).

Lens: SDK-author `.code` / `.message` / `.hint` clarity at every
fallible boundary + verify the unified F1 warn-shape contract that
landed mid-cycle (commits `7c6bd2ec` + `18aee490`) is honoured at HEAD.

---

## TL;DR — what landed since r10

**Resolved (or de-novo):**

- **[r10 §5 carry — CLOSED] `finalise_backfill` warn now carries
  `name` + `collection`.** `7c6bd2ec` added the two missing fields
  from lexical scope; `18aee490` further renamed `error` → `audit_err`
  and `terminal` → `transition` to match the other 5 F1 sites. At HEAD
  `migrations.rs:652-662` the field set is:
  `app_id = %app_id, name = %name, collection = %collection,
  audit_id = audit_id, transition = ?terminal, audit_err = %audit_err`.
  **6-cycle carry, closed.** **+1.0.**
- **[NEW] Unified F1 warn-shape contract across 6 sites
  (`7c6bd2ec` + `18aee490`).** All 6 audit-status warn sites
  (5 `update_audit_status` + 1 `finalise_backfill`) now use identical
  field names: `app_id = %app_id`, `audit_id`, `transition`,
  `audit_err = %audit_err` and identical message body
  `"update_audit_status failed; row stays in 'running' until reset"`
  (for the 5) or `"finalise_backfill failed; …investigate if the
  operator sees stuck migrations"` (for the 6th). The `transition`
  discriminator moved from message body (`"(Failed/data_violation)"`)
  to a structured field — a single grep `transition=Failed/` finds
  all 4 audit-row-Failed sites; `transition=Applied` finds the success
  site; `transition=?Failed(...)` (Debug-formatted enum) finds the
  finalise-backfill site. The shape contract is reviewer-enforced
  only — no tracing-subscriber snapshot test exists yet (Cargo.toml
  pulls in `tracing-subscriber` as dev-dep but no tests use it; see
  §4 NEW LOW). **+0.5.**
- **[NEW INFO] `apply.rs:Failed` still carries both `ddl_err` +
  `audit_err`.** The unification kept the primary-vs-secondary
  disambiguation introduced last cycle. Renamed `ddl_error` →
  `ddl_err` for cross-site consistency with `audit_err`. Pattern
  preserved.

**Carry from r10 (still open at HEAD `6ff294fd`):**

- **r10 §3 — `not_configured` 4-site overload.** Verified at HEAD:
  `exec.rs:68` / `:321`, `transaction.rs:147`, `auto_tx.rs:199` —
  identical to r10. **3rd cycle in carry.**
- **r10 §4 — `config_hinted()` + `validation_hinted()` zero callers.**
  Verified at HEAD: only 3 hits (1 doc + 2 declarations) in `error.rs`.
  **3rd cycle in carry** (5-cycle since r9 cohort).
- **r10 §6 #4 — `audit.rs:818` `invalid_app_id` missing alphabet.**
  Verified at HEAD `audit.rs:816-819`: still `"audit: invalid app_id:
  {name}"`. **4th cycle in carry** (now 3 in-tree exemplars to
  copy: `replication.rs:91-97`, `query.rs:131-133`, plus this cycle's
  uniform `transition` field as a structural precedent for inline-
  alphabet discipline).
- **r10 §8 — `auth/bootstrap.rs:1083-1090` no-op-prefix pin missing
  `backend_not_initialized` + `lazy_init_failed`.** Verified at HEAD:
  7 entries, both unifications since r9 still absent. **2nd cycle in
  carry.**
- r7 §7 `prefix_message` wildcard arm — unchanged.
- Cross-crate `toJSON` drops `hint` — out of native scope, 6th cycle.
- Cross-crate `withRetry` predicate narrow — out of native scope,
  6th cycle.

**New findings in r11:**

- **[NEW LOW] §4 — the unified F1 warn-shape contract is reviewer-
  enforced only.** `Cargo.toml:44` added `tracing-subscriber` as a
  dev-dep with a comment claiming it pins the 9+ structured warn shapes
  ("operator-grep-contract"); grep at HEAD shows zero `tracing_subscriber`
  imports in the test tree. The contract is documented in two places
  (Cargo.toml comment + reviewer commentary in `apply.rs:177` /
  `migrations.rs:647-651`) but not pinned. `18aee490`'s commit message
  explicitly cites "a snapshot test would have caught it pre-commit" —
  the gap remains. **2-LOC fix would be a single test that emits and
  asserts on field names**; multi-LOC fix would walk all 6 sites.
- **[NEW INFO] §3 — `finalise_backfill` `transition` field uses Debug
  formatting (`?terminal`) while the other 5 use string literals
  (`"Failed/index_build"` etc.).** `18aee490`'s commit message
  acknowledges this and argues the grep contract is on the field NAME
  not the value shape. In practice an operator searching
  `transition=Failed` matches the 4 string-literal Failed sites AND the
  enum-Debug `Failed(...)` site (because `Debug` for an enum variant
  prefixes the variant name). Acceptable but slightly noisy — listed
  for the next reviewer who might want strict uniformity.
- **[NEW VERIFIED] §5 — bench harness fail paths do NOT reach the
  SDK.** `compio-postgres::test_utils` is gated by the `test-utils`
  Cargo feature, only enabled under plugin-db's `[dev-dependencies]`.
  `row_for_test` either panics on bounded test inputs (column / value
  count mismatch, overflow on `usize → u16 / u32 / i32`) or returns
  `compio_postgres::Error`. Neither path is reachable from production.
  The user's concern in the prompt ("the bench harness adds new
  compio-postgres::test_utils fail paths — confirm these don't reach
  the SDK") is closed. **No deduction.**

**Net score delta vs r10:** see §9.

---

## 1. Verify cycle 12:47 / 13:17 / 13:47 / 14:17 closures end-to-end

### [PASS] `7c6bd2ec` — F1 warn-shape unification + finalise_backfill name/collection

Walked all 6 sites at HEAD. Shape contract verified:

| Site | `app_id` | `audit_id` | `transition` | `audit_err` | Extra |
|---|---|---|---|---|---|
| `apply.rs:178-184` (Applied) | `%app_id` | yes | `"Applied"` | `%audit_err` | — |
| `apply.rs:209-216` (Failed) | `%app_id` | yes | `"Failed"` | `%audit_err` | `ddl_err = %msg` |
| `backend/postgres.rs:496-503` (invalid_index) | `%app_id` | yes | `"Failed/invalid_index"` | `%audit_err` | `attempt` |
| `backend/postgres.rs:548-555` (data_violation) | `%app_id` | yes | `"Failed/data_violation"` | `%audit_err` | `sqlstate` |
| `backend/postgres.rs:597-604` (index_build) | `%app_id` | yes | `"Failed/index_build"` | `%audit_err` | `attempt, transient` |
| `migrations.rs:652-662` (finalise_backfill) | `%app_id` | yes | `?terminal` | `%audit_err` | `name, collection` |

**Message body uniform across the 5 `update_audit_status` sites:**
`"update_audit_status failed; row stays in 'running' until reset"`.

**Field shape uniform.** Operator grep `transition=` matches all 6;
grep `app_id=` works because every site uses `%` display formatting
(no shorthand-vs-display split, fixed this cycle). **PASS outright.**

### [PASS] `18aee490` — finalise_backfill warn-shape drift fix

Acknowledged in r10 §5 carry context. `7c6bd2ec` added `name` /
`collection`; `18aee490` finished the job by renaming `error` →
`audit_err` and `terminal` → `transition`. At HEAD the
finalise_backfill site lines up with the other 5 on field NAMES even
if `transition`'s value formatting (Debug enum) differs from the
string-literal pattern.

The 4-LOC drift would have been caught by a `tracing-subscriber`
snapshot test — Cargo.toml added the dep but no test landed
(see §4 NEW LOW).

### [PASS] `251d53b4` — `row_to_json` O(N²) → O(N)

Pure perf. The fallback semantics on each OID branch
(`Err(_) => Value::Null`) are unchanged — same JSON shape, same
NULL-on-decode-error contract the SDK sees. No error-UX impact.
**PASS.**

### [PASS] `f6adb68b` — IsolateDbContext field privatization

Pure visibility change. No error message touched. **PASS.**

### [PASS] `bac64c0e` — `mig_lock` accessor demotion

Pure visibility change. No error message touched. **PASS.**

### [PASS] `bf75e866` + `75d9ae5c` — test-utils + bench harness

Verified: `test-utils` is dev-dep-only on plugin-db; production
builds never see `row_for_test` / `column_for_test` /
`statement_for_test`. The only fail paths inside `test_utils.rs`
are panics on bounded test fixtures and a `compio_postgres::Error`
return from a malformed synthetic `DataRow` (which by construction
is well-formed in the bench). **PASS — no SDK leak.**

---

## 2. Verify r10 carry-overs at HEAD `6ff294fd`

### [CARRY] r10 §3 — `not_configured` overload

Grep at HEAD returns 4 production sites — unchanged from r10.
**3rd cycle in carry.** 6-LOC fix.

### [CARRY] r10 §4 — zero-caller constructors

Grep at HEAD: 3 hits (1 doc pointer, 2 declarations). No production
callers. **3rd cycle in carry.** 12-or-30 LOC fix (adopt or delete).

### [CARRY] r10 §6 #4 — `audit.rs:818` missing alphabet

Verified unchanged at HEAD. Now 3 in-tree exemplars to copy from
(`replication.rs:91-97`, `query.rs:131-133`, plus the cycle's new
`transition` field as a structural precedent for naming the rule).
**4th cycle in carry.** 1-LOC fix.

### [CARRY] r10 §8 — no-op-prefix pin missing 2 codes

Verified at `auth/bootstrap.rs:1083-1093` at HEAD: 7 entries,
`backend_not_initialized` and `lazy_init_failed` both absent.
**2nd cycle in carry.** 2-LOC fix.

---

## 3. Sample 4 NEW or REFINED warn sites — clarity audit

### #1 — `migrations.rs:652-662` — finalise_backfill (most-improved)

```rust
tracing::warn!(
    app_id = %app_id,
    name = %name,
    collection = %collection,
    audit_id = audit_id,
    transition = ?terminal,
    audit_err = %audit_err,
    "finalise_backfill failed; audit row may stay in 'running' \
     status until next reset() — investigate if the operator \
     sees stuck migrations"
);
```

The 6-cycle carry is closed by `name` / `collection` arriving + the
field shape being homogenised with the other 5 sites. The message
body carries the remediation (mention of `reset()` so the operator
knows the recovery primitive name). **Exemplar.**

### #2 — `apply.rs:209-216` — F1 audit-write Failed (rebadged)

```rust
tracing::warn!(
    app_id = %app_id,
    audit_id = id,
    transition = "Failed",
    ddl_err = %msg,
    audit_err = %audit_err,
    "update_audit_status failed; row stays in 'running' until reset",
);
```

The primary-vs-secondary-error pattern is preserved across the
rename (`ddl_error` → `ddl_err`, `audit_error` → `audit_err`). Still
the rail's most thoughtful warn — an operator sees in one line which
error came from DDL and which from the audit write, with a
`transition` discriminator that's stable for grep. **Exemplar.**

### #3 — `backend/postgres.rs:548-555` — F1 data-violation (rebadged)

```rust
tracing::warn!(
    app_id = %app_id,
    audit_id = id,
    transition = "Failed/data_violation",
    sqlstate = code_str,
    audit_err = %audit_err,
    "update_audit_status failed; row stays in 'running' until reset",
);
```

`sqlstate` field preserved on the rebadge. Operator can still
triage `23505` / `23502` / `23503` directly. The minor delta vs r10
sample: `transition` is now structured (was in message body); a grep
`transition=Failed/data_violation` is a stable token. **PASS.**

### #4 — `backend/postgres.rs:597-604` — F1 index_build (rebadged)

```rust
tracing::warn!(
    app_id = %app_id,
    audit_id = id,
    transition = "Failed/index_build",
    attempt,
    transient,
    audit_err = %audit_err,
    "update_audit_status failed; row stays in 'running' until reset",
);
```

`attempt` + `transient` preserved. Same retry-loop context as r10.
**PASS.**

**Sample observations:**
- 4 of 4 PASS. The unification preserved every operator-actionable
  field from the r10 sample while ratcheting up the grep contract.
- The single residual asymmetry is the finalise_backfill
  `transition = ?terminal` (Debug enum) vs the string-literal
  pattern on the other 5. Acceptable; tracked in §4 NEW INFO.

---

## 4. Retry semantics — unchanged from r10

| `.code` | Variant | Auto-retry? | Hint? | r11 verdict |
|---|---|---|---|---|
| `transient`, `serialization_failure`, `lock_not_available` | Transient/Serialization/LockContention | YES | YES | ✓ unchanged |
| `unique_violation` / `fk_violation` / `not_null_violation` / `check_violation` | constraint variants | NO | NO | ✓ unchanged |
| `validation_refused` | SchemaRefused | NO | envelope IS message | ✓ unchanged |
| `not_provisioned`, `wal_level_not_logical` | Configuration | NO | YES | ✓ unchanged |
| `lazy_init_failed`, `cic_configuration` | Configuration | NO | NO | ✓ unchanged |
| `not_configured` | Configuration | NO | NO | ⚠ overloads 2 conditions (r10 §3 carry, 3rd cycle) |
| `backend_not_initialized` | Configuration | NO | NO | ✓ unchanged |
| `invalid_identifier` | ValidationFailed via QueryError | NO | NO (message is remediation) | ✓ unchanged |

Pin at `error.rs:646-662` unchanged.

---

## 5. Concrete fixes ranked by SDK-author impact (r11)

| Rank | Finding | LOC | Files | Status vs r10 |
|---|---|---|---|---|
| 1 | r7 §10#1 — SDK `toJSON` drops `hint` | 3 | `sdks/db/src/collection.ts:46-65` | UNCHANGED (6th carry) |
| 2 | r7 §2.LOW — adopt `validation_hinted()` for 5 `session_*` codes | ~25 | `auth/session.rs:259` | UNCHANGED (6th carry) |
| 3 | r10 §3 — disambiguate residual `not_configured` overload | ~6 | `exec.rs:68,:321`, `transaction.rs:147`, `auto_tx.rs:199` | UNCHANGED (3rd carry) |
| 4 | r10 §6 #4 — name alphabet inline in `audit.rs:818` | 1 | `audit.rs:818` | UNCHANGED (4th carry) — now 3 in-tree exemplars |
| 5 | r10 §4 — adopt `config_hinted()` at 2 struct-literal sites OR delete | ~12 / ~30 | `wal_consumer.rs:349-358`, `replication.rs:277-286`, `error.rs:311-343` | UNCHANGED (3rd carry) |
| 6 | r11 §4 NEW — pin F1 warn-shape via `tracing-subscriber` snapshot | ~30 | `crates/plugin-db/tests/f1_warn_shape.rs` (new) | NEW (1st carry) |
| 7 | r10 §8 — add `backend_not_initialized` + `lazy_init_failed` to no-op-prefix pin | ~2 | `auth/bootstrap.rs:1083-1090` | UNCHANGED (2nd carry) |
| 8 | r6 §10#8 — augment `migrations.rs:329` "returned no row" with `(app_id, collection, name)` | ~5 | `migrations.rs` | UNCHANGED |
| 9 | r6 §6 / r7 §7 — replace `_ => {}` in `prefix_message` with explicit arms | ~10 | `error.rs:386` | UNCHANGED |
| 10 | r6 §10#10 — stamp `retryable: true` wire flag on `OpError::coded` | ~15 | `error.rs`, `runtime/src/core/state.rs` | UNCHANGED |
| 11 | r7 §10#4 — SDK `withRetry` predicate add 3 retryable codes | ~15 | `sdks/db/src/with-retry.ts` | UNCHANGED |
| 12 | r6 §10#11 — route `migrations::coded()` through `DbError::Coded` or delete | ~30 | `migrations.rs`, `error.rs` | UNCHANGED |

---

## 6. Score

**94.0 / 100** (+1.5 vs r10's 92.5)

**What earned the +1.5 this cycle:**

- **`7c6bd2ec` (F1 warn-half unification + finalise_backfill name/
  collection)** — closes the long-running 6-cycle carry on
  `finalise_backfill` AND ratchets the entire F1 warn family to a
  uniform field shape. Single biggest observability ratchet since
  the F1 warn-half landed at `fcf7ce3c` (r10 cycle). Real **+1.0.**
- **`18aee490` (F1 warn drift fix on the 6th site)** — closes
  the residual shape inconsistency `7c6bd2ec` missed by reflex
  ("would have been caught by a snapshot test"). The fact that the
  drift was caught one commit later by an internal reviewer (rather
  than landing in production) is itself the F1 warn-half doing its
  job. Real **+0.5.**
- **`251d53b4` / `f6adb68b` / `bac64c0e` / `bf75e866` / `75d9ae5c`** —
  zero error-UX delta but verified harmless. No score impact.

**What held the cycle back (no offsets, just unspent slack):**

- r11 §4 NEW: `tracing-subscriber` dev-dep added but no snapshot
  test of the F1 warn-shape contract. The contract is now
  reviewer-enforced; one drift per cycle is the demonstrated rate.
  +0.5 available next cycle.
- r10 §3 `not_configured` overload — 3rd cycle, no movement.
- r10 §6 #4 `audit.rs:818` alphabet — 4th cycle in carry, now 3
  exemplars to copy.
- r10 §4 zero-caller constructors — 3rd cycle, no movement.
- r10 §8 pin ratchet — 2nd cycle in carry.

**Why not higher (the -6.0 deficit, refreshed):**

- r10 §3 disambiguate `not_configured` — **+0.5–1.0** when split.
- r9 §2.LOW `validation_hinted` adoption (5 P0001 codes) — **+1.0.**
- r11 §4 `tracing-subscriber` F1 snapshot test — **+0.5.**
- r10 §4 constructor zero-caller adoption-or-delete — **+0.25.**
- r10 §6 #4 `audit.rs:818` alphabet — **+0.25.**
- r10 §8 no-op-prefix pin ratchet — **+0.25.**
- r7 §7 `prefix_message` wildcard arm — **+0.5** defensive.
- Cross-crate SDK gaps (`toJSON` hint drop + `withRetry` predicate
  narrowness) — block ceiling to ~96. Out of native scope.

**Trajectory:**

r2 (58) → r3 (71) → r4 (77) → r5 (85) → r6 (87) → r7 (91) → r8 (91)
→ r9 (91) → r10 (92.5) → r11 (94.0).

**Plateau still broken.** Two consecutive cycles of +1.5 each on the
back of the F1 warn-half family landing cleanly (`fcf7ce3c`
cycle 11:17, then `7c6bd2ec` + `18aee490` cycle 12:47–13:47). The
unification is the biggest observability win in 4+ cycles; the carry
items are all small (1–6 LOC) and unblocked.

**Forward projection:** if the next cycle lands ANY two of:
(a) §3 `not_configured` split, (b) §6 #4 `audit.rs:818` alphabet
(now 3 in-tree exemplars), (c) §4 NEW `tracing-subscriber` snapshot,
(d) §8 pin ratchet — the native rail should clear **95 / 100**. The
4 candidates are 11 LOC + ~30 LOC test, achievable in one sitting.
The ~96 ceiling without cross-crate work is still 5 cycles out at
current +1.5/cycle pace; +2.0/cycle would be possible if §4 +
§5 land together (zero-caller adoption + snapshot pin would move two
chronic carries in one go).
