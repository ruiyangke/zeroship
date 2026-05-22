# Plugin-db Test Coverage Audit — Round 6 (2026-05-22)

Re-audit, fresh.  Cycle following r5 (cycle 02:50 — **81/100**).  HEAD: `e24ac662`.

## Tool Results

- `cargo test -p zeroship-plugin-db --lib`: **348 passed, 0 failed, 0 ignored** in 0.14s.
- `cargo test -p zeroship-plugin-db --features test-helpers --test integration -- --list`: **73 tests**, 1 still `#[ignore]` (`p8a2_supervised_consumer_exits_on_slot_invalidated` at integration.rs:4175 — blocked on compio-postgres surfacing SQLSTATE 58P01, not flaky).
- Net lib delta since r5: **+12** (336 → 348). Matches the claimed +10 (I28 sweep) + +2 (4cbe9fa1 subscription).

Per-file `#[test]` counts (top of list):
| File | r5 | r6 | Δ |
| --- | ---: | ---: | ---: |
| `query.rs` | 147 | 147 | 0 |
| `context.rs` | 34 | 34 | 0 |
| `broker.rs` | 29 | 29 | 0 |
| `wal_consumer.rs` | 25 | 25 | 0 |
| `read_set.rs` | 20 | 20 | 0 |
| `replication.rs` | 9 | **14** | **+5** |
| `error.rs` | 10 | 10 | 0 |
| `diff.rs` | 11 | 11 | 0 |
| `v8_classes/migration.rs` | 8 | 8 | 0 |
| `auth/session.rs` | 8 | **9** | **+1** |
| `auth/keys.rs` | 2 | **4** | **+2** |
| `auth/bootstrap.rs` | 2 | **4** | **+2** |
| `orchestrator/lock_guard.rs` | 5 | 5 | 0 |
| `v8_classes/subscription.rs` | 0 | **2** | **+2** |

That accounts for the **12** new lib tests: 5 replication + 1 session + 2 keys + 2 bootstrap + 2 subscription = 12. Matches the I28 sweep claim (10) + I40 (2) within one — the discrepancy with the "10 new" framing is that two of the I28 tests landed in `replication.rs` as part of the `prefix_message` helper that didn't exist pre-sweep.

## 1. [I28] Sweep Test Discipline — 4-sample Audit

Sampled all 5 categories of new `Result<_, DbError>` guards.

### `replication.rs::sanitise_app_id_empty_returns_validation_failed` (line 771)

```rust
let err = sanitise_app_id("").unwrap_err();
match err {
    DbError::ValidationFailed { code, .. } => {
        assert_eq!(code, "invalid_app_id");
    }
    ...
}
```

**Verdict — strong.** Asserts both variant AND canonical `.code`. The sibling `sanitise_app_id_invalid_char_returns_validation_failed_with_code` (line 785) drives the assertion through `.to_op_error()` and matches on `OpErrorKind::CodedError { code, .. }` — i.e., it checks the **wire-level** code the SDK actually receives, not just the internal variant. **Best-shape test in the I28 batch.**

### `replication.rs::publication_and_slot_name_propagate_typed_error_code` (line 802)

Parametric: iterates `["", "has space"]` × `[publication_name, slot_name]` (4 cases). Each asserts `DbError::ValidationFailed { code == "invalid_app_id" }`. Compact, exhaustive over the input × function product. **Solid.**

### `replication.rs::prefix_message_preserves_variant_and_code` (line 829)

Loops over 4 variants (`Transient`, `LockContention`, `UniqueViolation`, `Internal`); calls `prefix_message(&mut variant, "replication: ctx: ")`; asserts (a) `.to_string()` starts with the prefix AND (b) `to_op_error()` keeps the expected `.code`. Followed by the **complement** test `prefix_message_leaves_structured_variants_alone` (line 879) which pins the no-op set (`Configuration`). **Excellent — covers both sides of the helper.**

### `auth/bootstrap.rs::coded_sql_no_op_branches_are_correct` (line 1085) — WEAK

```rust
let no_op_codes: &[&str] = &[
    "wal_level_not_logical", "not_configured",
    "session_signature_expired", "session_invalid_signature",
    "session_nonce_replay", "invalid_app_id",
];
assert!(!no_op_codes.is_empty());
```

The body comment is candid: *"The list above is a documentation guard, not an enforcement test."*  The assertion is `assert!(!no_op_codes.is_empty())` — i.e., it tests that the **test author wrote a non-empty array**. It does NOT call `coded_sql` and does NOT verify any branch behaviour. If `coded_sql` is refactored to prefix one of the structured codes, this test still passes.

```
[MEDIUM] crates/plugin-db/src/auth/bootstrap.rs:1085 — coded_sql_no_op_branches_are_correct is documentation-only
  Why: lists 6 no-op codes in an array but the only assertion is array-non-empty. The bug class
       (prefix-eligible reclassification of structured variants) is NOT actually pinned. This
       test counts toward the +10 claim but contributes zero regression-detection.
  Fix: either (a) construct each variant and call coded_sql via a feature-gated test seam,
       or (b) downgrade the test to a `// invariant note:` comment and stop counting it.
  Verification: grep -n "assert" crates/plugin-db/src/auth/bootstrap.rs:1085-1106
```

### `auth/keys.rs::keys_internal_parse_error_stamps_internal_code` (line 232)

```rust
let err = DbError::internal("auth/keys: parse current id: invalid digit found");
let op = err.to_op_error();
match op.kind {
    OpErrorKind::CodedError { code, .. } => assert_eq!(code, "internal"),
    ...
}
```

This tests `DbError::internal(...).to_op_error().code == "internal"` — i.e., it tests the **DbError → OpError** mapping, NOT the `current_key_id` / `previous_key_id` parse paths themselves. The fix added `.code = "internal"` to those parse-error sites; the test only proves the variant produces the right code in isolation. If a future refactor changes the parse-error site to `DbError::Configuration { code: "x" }` the test still passes (because the test never calls `current_key_id`). **Weak but not broken — pins the mapping, not the call-site.**

### `auth/session.rs::session_helpers_signatures_are_typed` (line 466)

Type-level signature guard. Four `fn _xxx(...) -> impl Future<Output = Result<_, DbError>>` bindings. Useful as a compile-fail tripwire if any of the four helpers regresses to `Result<_, String>`. Zero runtime assertion. Fine for its purpose; **type-level only, value behaviour deferred to integration.**

### `auth/keys.rs::keys_helpers_signatures_are_typed` (line 208) & `auth/bootstrap.rs::bootstrap_signature_is_typed` (line 1067)

Same shape — type-level signature pins. **Fine.**

### Integration upgrade (b8c_init_session_*)

Lines 3608, 3645, 3690 — the three pre-existing `b8c_init_session_rejects_*` integration tests were extended with a typed-variant assertion block:

```rust
match err {
    DbError::ValidationFailed { code, .. } => {
        assert_eq!(code, "session_signature_expired");
    }
    other => panic!("expected ValidationFailed, got: {other:?}"),
}
```

These DO exercise the actual SECURITY DEFINER → SQLSTATE → DbError pipeline against a live Postgres. **Surgical and correct.**

### I28 Batch Summary

| Test | Variant assert | Code assert | Wire-OpError assert | Calls the fixed code |
| --- | :-: | :-: | :-: | :-: |
| `sanitise_app_id_empty_returns_validation_failed` | yes | yes | no | yes |
| `sanitise_app_id_invalid_char_returns_validation_failed_with_code` | no | yes | yes | yes |
| `publication_and_slot_name_propagate_typed_error_code` | yes | yes | no | yes |
| `prefix_message_preserves_variant_and_code` | (4 variants) | yes | yes | yes |
| `prefix_message_leaves_structured_variants_alone` | yes | yes | yes | yes |
| `session_helpers_signatures_are_typed` | (type only) | (type only) | no | no |
| `keys_helpers_signatures_are_typed` | (type only) | (type only) | no | no |
| `keys_internal_parse_error_stamps_internal_code` | n/a | yes | yes | **no** |
| `bootstrap_signature_is_typed` | (type only) | (type only) | no | no |
| `coded_sql_no_op_branches_are_correct` | **no** | **no** | **no** | **no** |
| `b8c_init_session_rejects_expired_token` (extended) | yes | yes | no | yes |
| `b8c_init_session_rejects_replay_nonce` (extended) | yes | yes | no | yes |
| `b8c_init_session_rejects_tampered_signature` (extended) | yes | yes | no | yes |

**8 / 13 are real behaviour tests; 4 are type-level signature pins (acceptable shape); 1 is a no-op assertion (`coded_sql_no_op_branches_are_correct`).** Discipline is meaningfully higher than r5's auto_tx batch (which tested 2 of ~8 variants). The `prefix_message` pair is the cleanest test shape in plugin-db.

## 2. [I42] Lock Guard Reorder — New Branch NOT Covered

```rust
// post-bd1e7ce1 release() body:
if let Some(client) = self.client.as_ref() {       // new branch
    let unlock_sql = "SELECT pg_advisory_unlock(...)";
    let _ = client.query_text_params(unlock_sql, ...).await;
}
self.released = true;                              // moved AFTER await
Ok(self.client.take())
```

Existing 5 tests inventory:
1. `release_idempotent_when_no_client` — `client: None` → skips the `if let Some(...)` branch entirely.
2. `into_held_flips_released_flag` — does not call `release()`.
3. `drop_with_released_true_does_not_warn` — manually flips `released`; no release call.
4. `drop_with_released_false_runs_warning_branch` — drops un-released guard; no release call.
5. `released_flag_starts_false` — field-check.

**None of the 5 tests executes the new `&`-borrow → await → flip-then-take path.** The 5 tests still pass (r6 lib run confirms), but the [I42] reorder itself — the defining behaviour of bd1e7ce1 — is unobserved at unit level. The bug class (await cancellation between `released = true` and the unlock SQL) cannot be reproduced without:
- a `PooledClient` constructed against a live pool (impossible in `--lib`), AND
- a way to drop the release future mid-await (compio's no-cancellation model makes this even harder).

```
[HIGH] crates/plugin-db/src/orchestrator/lock_guard.rs:124–149 — [I42] reorder unverified
  Why: The fix's defining behaviour (await-then-flip) requires a live PooledClient and a
       cancellable runtime. No test in the suite hits the new branch. A regression that
       reverts the order would not fail any test — only catastrophic-path field state is
       pinned, not the cause that triggers it.
  Fix: integration test that (a) acquires the guard, (b) wraps release() in a
       futures::future::poll_fn that returns Pending once then drops the future,
       (c) re-queries pg_locks for the (key,tag) tuple and asserts it's STILL held
       (cancellation case) or NOT held (clean-release case). Probe-on-Postgres, not unit.
  Verification: grep -n "as_ref\|cancel\|drop_mid" crates/plugin-db/src/orchestrator/lock_guard.rs
                → only the production code mentions cancellation; no test.
```

The commit message claims "338 passed" — true, but only because the 5 tests don't exercise the changed lines.

## 3. [I40] Subscription Leak — Structural + Live V8 Pair

`crates/plugin-db/src/v8_classes/subscription.rs` (was 0 tests, now 2):

### `mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure` (line 288) — STRUCTURAL

```rust
let body = mint_subscription_body();    // extracts the source between matching braces
let subscribe_pos = body.find("broker::subscribe(").expect(...);
// walks the source looking for `?` operators, ignoring `//` line comments
// asserts every `?` position is < subscribe_pos
```

A **source-text invariant**: every `?` (early-return) must syntactically precede the `broker::subscribe(` call. This is a clever non-runtime way to pin the bug class — a future refactor that moves the subscribe above any `?` fails the test immediately, even without a V8 isolate.

**Caveats:**
- Brittle to harmless refactors: renaming `broker::subscribe` to a re-export breaks the test. Adding a `?` inside a block comment **after** subscribe (currently impossible because `//` is the only comment style skipped, but a `/* … ? … */` would slip through) would create a false positive.
- The function's source is found via `include_str!` and walked with a hand-rolled brace counter (no string-literal awareness). For the current body this is fine.

**Verdict — solid structural assertion. The right shape for a structural-invariant test the user explicitly asked for.**

### `mint_subscription_happy_path_registers_exactly_one_broker_entry` (line 345) — REAL V8

Spawns a fresh OS thread, inits V8, creates an isolate + context, calls the real `mint_subscription("test_app_unit", "messages")`, asserts `broker::live_subscription_count() == 1` immediately, then forces GC + microtask checkpoint and asserts the count is 0. **Counterpart to the structural test — proves the assertion above isn't vacuously true.**

Two minor concerns:
1. `broker::live_subscription_count()` is thread-local (broker is per-thread) — the fresh thread guarantees isolation, good.
2. `request_garbage_collection_for_testing` + `perform_microtask_checkpoint` is the standard V8 GC dance, but the test would race the finalizer if V8 ever changes its finalizer scheduling. Tolerable.

**This pair is the strongest new test addition of the cycle.**

## 4. [I41] Backfill Race — Zero Test Coverage

37e61803 moves `update_backfill_progress` BEFORE COMMIT to keep the FOR UPDATE row lock held while the cursor write lands. The commit message admits: *"cargo test → 336 passed"* — i.e., **no new test was added**.

```
[MEDIUM] crates/plugin-db/src/migrations.rs:580–600 — [I41] backfill race fix unverified
  Why: The race window between exec_commit_batch's COMMIT and update_backfill_progress
       is exactly the kind of bug that requires a concurrent reset() + commit_batch to
       reproduce. No test exercises this.
  Fix: integration test that spawns (a) a long-running commit_batch with a slow row UPDATE
       and (b) a migrations.reset() concurrently; asserts the post-COMMIT progress matches
       the data state (cursor never strands).
  Verification: git show 37e61803 -- crates/plugin-db/tests/ → empty diff.
                grep -rn "update_backfill_progress\|reset.*concurrent" crates/plugin-db/tests/
                → no concurrent-test hits.
```

## 5. Branches Still Not Covered (R3 + R4 + R5 Carry)

### `create_index_with_recovery_audited` — R3+R4+R5 carry

`crates/plugin-db/src/backend/postgres.rs:385–600` — 215-line retry loop, 6 terminal branches:
- `Ok` + `valid` → return `Ok(())` (happy path)
- `Ok` + `!valid` + `attempt < MAX_RETRIES` → log + drop + retry
- `Ok` + `!valid` + `attempt == MAX_RETRIES` → `SchemaRefused { validation_refused }`
- `Err(fatal)` (23505/23502/23503/23514) → `SchemaRefused { unique_violation }`
- `Err(transient)` (40P01/53100/53200) + `attempt < MAX_RETRIES` → retry
- `Err(transient)` + `attempt == MAX_RETRIES` OR `Err(!transient)` → `SchemaRefused { validation_refused, sqlstate }`
- fall-through invariant breach → `Configuration { cic_configuration }`

The integration test `a1_unique_index_actually_enforces_uniqueness` (integration.rs:858) exercises ONLY the `Ok + valid` path against a live PG. The 5 non-happy branches remain **completely uncovered** since R3.

```
[HIGH] crates/plugin-db/src/backend/postgres.rs:385–600 — create_index_with_recovery_audited
       still has 5 uncovered terminal branches (R3+R4+R5 carry, now r6).
  Why: 215-line retry loop with 6 terminal exits and only 1 covered. The error envelopes
       (`cic_failed`, `validation_refused`, `cic_configuration`) are part of the SDK contract.
  Fix: 5 unit tests against a mocked Backend stub (Backend trait already exists per
       crates/plugin-db/src/backend/mod.rs — substitute a stub Pool that returns scripted
       Result<Vec<Row>, pg::Error> per call).
  Verification: grep -rn "cic_failed\|validation_refused\|cic_configuration"
                crates/plugin-db/tests/ → 0 hits.
```

### `validate_field_name` Unicode boundary — R5 carry

`validate_field_name` (query.rs:106) blocks empty + null + >63 bytes. It does NOT enforce ASCII-only (only `validate_collection` does at line 90–97). A multibyte UTF-8 field name like `"café".repeat(16) + "x"` (63 bytes, ~32 code points) passes validation and reaches `quote_ident` — which `replace('"', "\"\"")` does not Unicode-normalize. Postgres treats `"café"` and `"cafe\u{0301}"` as distinct identifiers.

```
[MEDIUM] crates/plugin-db/src/query.rs:106 — validate_field_name lacks Unicode boundary tests
  Why: `name.len()` is bytes (correct for NAMEDATALEN), but no test verifies behaviour at
       the boundary with multi-byte UTF-8 (e.g., 63 bytes of 2-byte chars = 31.5 code points
       → invalid UTF-8 boundary potential; or NFC/NFD aliasing).
  Fix: parametric test:
        - "α".repeat(31) + "z" (63 bytes, valid UTF-8, no allowlist failure) → should it pass?
        - "α".repeat(32) (64 bytes) → must reject as oversized.
        - "café" vs "cafe\u{0301}" → distinct identifiers, doc the policy.
  Verification: grep -nE "(unicode|multibyte|utf8|chars\(\)\.count)" crates/plugin-db/src/query.rs
                → 1 hit (only a "Safety: ALPHABET is ASCII" comment, no test).
```

### Concurrent registerModel — R4+R5 carry

`integration.rs::a3_concurrent_register_model_via_advisory_lock` (line 1322) still explicitly admits it's sequential. No change since r5.

### Mid-stream connection drop on subscribe — R5 carry

No integration test for `subscribe()`'s AsyncIterable + abrupt peer disconnect.

## 6. Files Still Bare (R4+R5 Carry)

| File | LOC | `#[test]` count | Status |
| --- | ---: | ---: | --- |
| `crates/plugin-db/src/crud.rs` | 584 | **0** | UNCHANGED since R4 |
| `crates/plugin-db/src/v8_bridge.rs` | 497 | **0** | UNCHANGED since R4 |
| `crates/plugin-db/src/v8_classes/collection.rs` | 393 | **0** | UNCHANGED since R4 |
| `crates/plugin-db/src/v8_classes/transaction.rs` | 336 | **0** | UNCHANGED since R4 |
| `crates/plugin-db/src/replication_ops.rs` | 285 | **0** | UNCHANGED since R4 |
| `crates/plugin-db/src/v8_classes/migrations.rs` | 289 | **0** | UNCHANGED |
| `crates/plugin-db/src/orchestrator/transaction.rs` | 173 | **0** | UNCHANGED |

**Total bare LOC: 2,557** (up from 1,810 because r6 surfaced 3 additional bare files not listed in r5: `replication_ops.rs` (285), `v8_classes/migrations.rs` (289), `orchestrator/transaction.rs` (173)).

```
[HIGH] crates/plugin-db — 7 source files totalling 2,557 LOC still at 0 unit tests
  Why: 4 are V8-bridge files where coverage requires V8 init + scope (the I40 subscription
       commit proved this is feasible — see `mint_subscription_happy_path_registers_exactly
       _one_broker_entry`). 3 are pure-logic files (crud.rs path-builders, replication_ops.rs
       dispatch helpers, orchestrator/transaction.rs) where conventional unit tests are
       straightforward.
  Fix:
    - crud.rs: split out the path-builder logic into pure fns + test it (no V8 needed).
    - v8_bridge.rs: follow the subscription-test pattern — fresh-thread V8 init.
    - v8_classes/{collection,transaction,migrations}.rs: same pattern.
    - replication_ops.rs: signature-level type pins (cheapest discipline win) + dispatch
      table existence checks.
  Verification: grep -c "#\[test\]" crates/plugin-db/src/crud.rs crates/plugin-db/src/v8_bridge.rs
                crates/plugin-db/src/v8_classes/{collection,transaction,migrations}.rs
                crates/plugin-db/src/replication_ops.rs
                crates/plugin-db/src/orchestrator/transaction.rs → all 0.
```

## 7. Stale Test Discovered

After 91830cca dropped the `.into_string()` coercion from `ensure_publication_and_slot`, the test `empty_returning_string_shape_keeps_replication_prefix` (replication.rs:711) has a **stale doc comment**:

```rust
/// `ensure_publication_and_slot` returns `Result<_, String>` (not
/// `Result<_, DbError>` like audit.rs), so the runtime fix calls
/// `.into_string()` on the `DbError::Internal` before flowing it
/// through `?`.
```

The function now returns `Result<_, DbError>` directly; the test still asserts `into_string()` output but does so against a **manually-constructed** `DbError`, not the production code path. The test passes but no longer tests what it claims to test.

```
[LOW] crates/plugin-db/src/replication.rs:704–729 — stale test doc-comment and decoupled assertion
  Why: post-91830cca, the test asserts the shape of DbError::Internal::into_string() in
       isolation. The production code no longer calls into_string() (the closure now returns
       DbError directly). A future caller could legitimately drop into_string() from the
       error rail entirely and this test would still pass.
  Fix: either (a) inline the actual production closure call and test ?-propagation through
       a stub, or (b) downgrade to a value test of DbError::Internal's Display impl in error.rs.
  Verification: git show 91830cca -- crates/plugin-db/src/replication.rs
                → into_string() removed from production but test doc still references it.
```

## 8. Integration Suite Stability

- **73 listed** (no change since r5).
- **1 `#[ignore]`** — unchanged blocker (`p8a2_supervised_consumer_exits_on_slot_invalidated`, integration.rs:4175). Note: `--list` does NOT run the tests, so flake observation requires `cargo test --test integration` against a live PG — out of scope for this read-only audit, but the listing pass alone is stable (no compile errors, no missing fixtures).
- **No changes to integration tests this cycle** beyond the 3 `b8c_init_session_*` extensions in 0049d9be.

## Summary

**Strengths since r5:**
- I28 sweep: 8 of 13 new/extended tests are real-behaviour value+code+wire assertions; 4 are acceptable type-level pins.
- `prefix_message` test pair is the cleanest shape in plugin-db.
- I40 subscription tests pair a structural-source invariant with a real V8 happy-path — exactly the shape the user asked for in the previous cycle ("structural assertion of the V8-alloc-failure path").
- Lib count 336 → 348 (+12).

**Weaknesses (carries + new):**
- **`coded_sql_no_op_branches_are_correct` is documentation-only** (the only assertion is `!arr.is_empty()`). Counts toward I28's "10 new tests" claim but contributes no regression-detection.
- **[I42] lock_guard reorder unverified.** None of the 5 existing tests hits the new `if let Some(client) = self.client.as_ref()` branch — they all use `client: None`. The await-then-flip ordering, the defining behaviour of bd1e7ce1, has zero test coverage.
- **[I41] backfill race fix has zero tests.** Move-before-COMMIT is plausible-on-paper but the failure mode (reset clobber during commit) requires concurrency to reproduce — no test does.
- `create_index_with_recovery_audited`: 5 of 6 terminal branches still uncovered (R3+R4+R5 carry).
- `validate_field_name` Unicode 63-byte boundary still untested (R5 carry).
- Bare-file count grew from 4 (r5: 1,810 LOC) to 7 (r6: 2,557 LOC) — three previously unflagged files surfaced.
- Stale `empty_returning_string_shape_keeps_replication_prefix` doc + assertion.

**Score: 82 / 100** (r5: 81). Net **+1**:
- **+3** I28 sweep raises the bar on `.code` preservation discipline (8/13 strong shapes; `prefix_message` pair is exemplary).
- **+2** I40 structural+V8 pair is exactly the right shape; sets a pattern other V8-bridge files should follow.
- **−1** `coded_sql_no_op_branches_are_correct` is a documentation-only test counted as real coverage.
- **−1** [I42] lock_guard reorder branch unverified — the fix's defining behaviour has no test.
- **−1** [I41] backfill race has zero tests despite shipping.
- **−1** Bare-file inventory grew to 7 files / 2.5K LOC.

The cycle's net direction is positive — the I40 structural+live pair and the `prefix_message` shape are durable improvements — but two of the four [I*] fixes (I41, I42) shipped without test coverage of the fix's defining behaviour, which is a discipline regression that offsets most of the gains.
