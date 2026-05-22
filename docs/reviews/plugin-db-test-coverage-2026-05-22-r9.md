# plugin-db — Test Coverage Review (round 9)

- **Date:** 2026-05-22
- **Scope:** `crates/plugin-db/` (src + tests/)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84)
- **Method:** Read-only. Ran `cargo test -p zeroship-plugin-db --lib`, counted
  `#[test]` / `#[compio::test]` per file, inspected r8 carry-overs and the four
  cycle-09:00 commits since r8.

---

## TL;DR

Round-on-round delta is **small but real**. Since r8:

- One new behavioural test landed (`wal_consumer_new_missing_db_url_returns_configuration` at `crates/plugin-db/src/wal_consumer.rs:977`) covering the *hint-is-Some* leg of the f1c5184e Configuration hint field — a focused, well-targeted test.
- Three of the four commits since r8 (3d79d2da, 09e32998 + bc4363f0, 9e392ba1) are documentation / surface refinements. Their *correct* test-impact is zero — they're not bug fixes and don't change observable behaviour.
- Lib test count moved from 364 (r8) → **371**. Integration suite stable at **73 + 1 #[ignore]**.

The two persistent long-tail gaps from r6 onward are **unchanged**: bare-files cluster (4 modules, 1,815 LOC, zero unit tests) and `create_index_with_recovery_audited` (5+ uncovered branches). Neither is a *new* problem but neither has moved.

Net: **+0** vs r8 (84 → 84). The hint test is good, the doc commits are appropriately test-neutral, but no movement on the persistent gaps. Holding pattern.

---

## 1. Lib test count + per-file breakdown

```
cargo test -p zeroship-plugin-db --lib
…
test result: ok. 371 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out
```

Lib tests: **371 passing** (was 364 at r8, 358 at r7). All green. No flakes.

Per-file `#[test]` / `#[compio::test]` count vs LOC:

```
tests   LOC    file
 147   4277   src/query.rs
  34    943   src/context.rs
  29   1311   src/broker.rs
  26   1352   src/wal_consumer.rs           ← +1 since r8 (hint test)
  20    583   src/read_set.rs
  16    607   src/auth/session.rs
  14    958   src/replication.rs
  14    880   src/error.rs
  11   1107   src/diff.rs
  10    341   src/v8_classes/replication.rs
   8    848   src/v8_classes/migration.rs
   6    383   src/orchestrator/lock_guard.rs
   4    894   src/audit.rs
   4    425   src/replication_ops.rs
   4    384   src/orchestrator/auto_tx.rs
   4    231   src/auth/keys.rs
   4   1100   src/auth/bootstrap.rs
   3    958   src/migrations.rs
   3    491   src/exec.rs
   3    481   src/v8_classes/db.rs
   3    396   src/orchestrator/register_model/apply.rs
   2    726   src/backend/postgres.rs
   2    446   src/backend/mod.rs
   2    378   src/v8_classes/subscription.rs
   0    585   src/crud.rs                   ← bare, unchanged since r6
   0    497   src/v8_bridge.rs              ← bare, unchanged since r6
   0    393   src/v8_classes/collection.rs  ← bare, unchanged since r6
   0    340   src/v8_classes/transaction.rs ← bare, unchanged since r6
```

Bare-files cluster: 1,815 LOC across 4 modules, **0 unit tests** between them. Same set as r6, r7, r8.

---

## 2. Integration suite stability

```
grep -cE '#\[test\]|#\[compio::test\]' tests/integration.rs   → 73
grep -c '#\[ignore'                    tests/integration.rs   → 1
```

73 active + 1 `#[ignore]`. **No change** since r6.

The lone `#[ignore]` at `integration.rs:4175` is still there. Round-9 has no new commit affecting it; carrying it forward as documented technical debt is consistent with the prior rounds' assessments and does not warrant a downgrade this round.

---

## 3. R8 carry-overs

### 3.1 `create_index_with_recovery_audited` — 5+ uncovered branches

**Status: unchanged.** Zero direct tests, zero references in `tests/integration.rs`.

```
$ grep -rn create_index_with_recovery_audited crates/plugin-db/
src/backend/postgres.rs:356:        create_index_with_recovery_audited(...)
src/backend/postgres.rs:385:async fn create_index_with_recovery_audited(...)
$ grep -n create_index_with_recovery_audited tests/integration.rs
(no matches)
```

The function (`crates/plugin-db/src/backend/postgres.rs:385-601`) still owns the
CIC retry logic with these uncovered branches:

| line  | branch                                                            | covered |
| ----- | ---------------------------------------------------------------- | ------- |
| 471   | success → `indisvalid = true` → `return Ok(())`                  | ❌      |
| 488   | INVALID index re-landed; final retry; `refuse(validation_refused)` | ❌      |
| 510-538 | fatal SQLSTATE (UNIQUE/NOT_NULL/FK/CHECK) → drop + `refuse(unique_violation)` | ❌ |
| 541-546 | transient SQLSTATE (DEADLOCK / DISK_FULL / OOM) → retry         | ❌      |
| 570-583 | non-transient OR retry-budget-exhausted → `refuse(validation_refused)` | ❌ |
| 593-600 | loop-exit invariant breach → `Configuration { code: "cic_configuration", hint: None }` | ❌ |
| 411-422 | `refuse` envelope-serialization fallback string                  | ❌      |

`backend/postgres.rs` lib-side already documents (lines 605-640) that the
PostgresBackend facade can only be covered via integration tests because every
async method needs a live pool. That argument is **valid for the facade** but
*not* for `create_index_with_recovery_audited`'s classification logic, which is
mostly a `match` on `SqlState` codes and a `refuse(...)` envelope builder. The
classification table (fatal-set membership, transient-set membership, terminal
vs retry decision) is pure logic and is unit-testable by extracting it into a
free function that takes a `Option<SqlState>` and returns an enum. That is a
revision call, not a critique call — flagging only as the same r5/r6/r7/r8
finding restated for r9.

### 3.2 Bare files (4) — pick one for deep audit: `v8_classes/transaction.rs`

**Status: unchanged.** Four files, 1,815 LOC, zero unit tests.

Deep audit on `crates/plugin-db/src/v8_classes/transaction.rs` (340 LOC, 0 unit tests, 0 deep coverage from integration):

**Uncovered branches that do NOT need a live PG client to exercise:**

1. **`#[v8_constructor]` `Transaction::new()` always rejects** (line 154-157)
   - "Illegal constructor" error path.
   - Grep: `grep -rn 'Illegal constructor.*Transaction' tests/` → no matches.
   - Trivially testable with the same v8 test scaffold that `db_v8_class.rs` already uses for `db_brand_check_rejects_non_db`.

2. **`tx.collection("")` validation** (line 172-176)
   - "tx.collection: name must be a non-empty string" → `OpError::type_error`.
   - No test asserts the empty-name rejection.
   - Same scaffold: build a v8 isolate, mint a Transaction with token, call `.collection("")`, assert the thrown error message.

3. **`tx.collection(name)` after settled** (line 177-183)
   - Returns `DbError::validation("tx_settled", ...)`.
   - Not asserted anywhere.

4. **`tx.collection(name)` cache hit returns cached object** (line 184-186)
   - Pure cache behaviour, no DB needed; assert object identity across two calls.
   - Mirrors the `db_collection_caches_by_name` test pattern at `tests/db_v8_class.rs:55` — same approach would lift this branch to covered.

5. **`Transaction::Drop` token=0 / settled early-return** (line 121-123) — pure-Rust, no V8 scope mid-drop; assertable with a directly-constructed `Transaction` struct after flipping `settled=true`. Currently uncovered.

6. **`Transaction::Drop` token-mismatch early-return** (line 124-127) — assertable by stamping a stale token that doesn't match `IsolateDbContext::tx_token()`. Currently uncovered.

7. **`end()` settled-already short-circuit** (line 226-228) — Ok(()) without re-running SQL. Uncovered (integration tests always settle exactly once).

8. **`end()` concurrent-settle current ≠ token branch** (line 229-233) — sets `settled = true` and returns Ok(()) without running ROLLBACK. Uncovered.

9. **`end()` "tx_conn already cleared by another path"** (line 240-246) — early-return when `take_tx_client()` returns `None`. Uncovered.

10. **`end()` clears pending emits on ROLLBACK / COMMIT-failure** (line 266-270) — important for the deferred-broker contract (subscribers must not see rolled-back writes); only the COMMIT-success path is exercised end-to-end via `auto_tx.rs::t1_handler_success_commits`. The clear-on-failure path is `assert!(crate::exec::pending_emits_count() == 0)` after a forced failure; no test enforces it directly.

`auto_tx.rs` covers the *happy* COMMIT/ROLLBACK behaviour end-to-end (success commits, reject rolls back, mutation visible after commit, rollback envelope, etc., 8 tests) but does not exercise the GC/Drop finalizer-path or the concurrent-settle races that exist precisely because Transaction has both a Drop and an `end()` path.

### 3.3 `lock_guard` release SQL path — integration coverage status

**Verdict: closed at integration level.** The r8 "integration-test path still needed?" question resolves to **NO new integration test needed** — the integration suite already exercises the full acquire → unlock-SQL → release path:

- `tests/integration.rs:1382-1402` (in test 30, `a2_concurrent_deploys_serialize_under_lock`) explicitly verifies the lock is **acquired and released** by checking `pg_try_advisory_lock` succeeds after `exec_register_model_with_pool` returns. That call path goes through `OrchestratorLockGuard::release().await` → `pg_advisory_unlock` SQL → return-to-pool, so the live unlock SQL *is* covered.

- The lib-side adds 6 unit tests (the source-structural invariant at `lock_guard.rs:349` from b6ae… plus the 5 lifecycle tests) which pin the cancellation-safety ordering.

The remaining gap inside `release().await` is the **unlock-SQL error branch** at `lock_guard.rs:168-179` (the `tracing::warn!` when `query_text_params` fails). Driving the underlying client to fail mid-await would need a fault-injection backend (close the client between acquire and release). This is a `[INFO]` — not actionable in the current backend abstraction.

---

## 4. Coverage of cycle 09:00 closures

Audited each commit since r8 (`f1c5184e`, `3d79d2da`, `09e32998`, `bc4363f0`, `9e392ba1`):

### 4.1 `f1c5184e` — Configuration hint field; one new test

The commit added `wal_consumer_new_missing_db_url_returns_configuration`
(`src/wal_consumer.rs:977-990`). The test correctly asserts:
- `code == "not_provisioned"`
- `hint.is_some()`
- `message.contains("db_url")`

This is a well-scoped behavioural test for the *new* invariant (Configuration carries a hint).

**However**, the hint field is a *cross-cutting* change. Other `hint: Some(...)` Configuration sites exist that were NOT touched in this round's tests:

```
$ grep -rn 'hint: Some' crates/plugin-db/src/
src/replication.rs:282          ← Configuration { code: "wal_level_not_logical", hint: Some(...) }
src/wal_consumer.rs:352         ← Configuration { code: "not_provisioned", hint: Some(...) }   (covered ✓)
src/error.rs:319                ← `config_hinted` constructor
src/error.rs:341                ← `validation_hinted` constructor
src/error.rs:575                ← test assertion already
src/error.rs:821                ← test assertion already
```

The **`replication.rs:278-286 wal_level_not_logical` hint** is asserted by **no test**. `tests/integration.rs:2781` references `wal_level=logical` only as a skip-comment, not as an assertion. `error.rs:786 prefix_message_leaves_structured_variants_alone` uses `hint: None` for the wal_level_not_logical variant, so the hint string is invisible there. If someone changed the hint string (or removed it) on `replication.rs:283`, no test would catch it. See `[MEDIUM]` gap below.

The hint *constructors* `config_hinted` and `validation_hinted` (`error.rs:311-321`, `333-343`) have **no direct unit test** either — they're constructed and assertions exist for *callers*, but the constructors themselves (`code` + `message` + `hint = Some(...)` shape) are only tested transitively.

### 4.2 `3d79d2da` — Docs-audit fixes (test-neutral)

Docs-only. Correctly carries no test changes.

### 4.3 `09e32998` + `bc4363f0` — TX_CONN sweep + OBJECT_PREFIX demote

`TX_CONN` sweep: refactor / doc cleanup, no observable behaviour change. Correctly test-neutral.

`OBJECT_PREFIX` demote: surface-level demotion (from one visibility tier to a smaller one). Pure surface refactor, no semantic change. Correctly test-neutral.

### 4.4 `9e392ba1` — error.rs preamble

Module preamble doc rewrite. Correctly test-neutral.

**Cycle 09:00 closures audit summary:** ✓ test-neutral commits are appropriately test-neutral; the hint field test is correctly scoped to the new invariant but leaves one sibling site (`replication.rs:282`) unverified.

---

## 5. Findings

### [MEDIUM] crates/plugin-db/src/replication.rs:282 — `wal_level_not_logical` hint string is not asserted by any test

  Why: The Configuration-hint-Some pattern that f1c5184e introduced as a wire contract has two production sites in src/. One is covered by the new wal_consumer test; the other (`replication.rs:282 set wal_level=logical in postgresql.conf and restart`) is asserted nowhere.

  Fix: Add a unit test in `src/replication.rs::tests` (or extend `tests/integration.rs` test 49+) that:
  - Asserts `replication::setup`'s `wal_level_not_logical` branch returns `Configuration { hint: Some(s) }` where `s.contains("wal_level=logical")` and `s.contains("postgresql.conf")`.
  - If the hint string drifts, the test fails before the SDK starts surfacing the wrong remediation guidance.

  Verification:
  ```
  $ grep -rn 'wal_level=logical' crates/plugin-db/{src,tests}/
  src/replication.rs:283: "set wal_level=logical in postgresql.conf and restart"
  src/error.rs:309-310:  /// surface verbatim (e.g. "set wal_level=logical in postgresql.conf and restart")
  tests/integration.rs:2781-2785: # skip-comment only, no assertion
  ```
  Three sites mention the string; only the production constant and a doc comment reference it. No assertion.

### [MEDIUM] crates/plugin-db/src/error.rs:311,333 — `config_hinted` / `validation_hinted` constructors lack direct unit tests

  Why: New convenience constructors introduced in the hint-field round. Their invariant — `code`, `message`, AND `hint: Some(_)` all flow through — is verified only transitively (by callers that happen to use them). A regression that flipped the `Some(hint.into())` to `None` would not fail any test directly in `error.rs::tests`.

  Fix: Add two trivial unit tests (`config_hinted_sets_all_fields`, `validation_hinted_sets_all_fields`) constructing the helper, pattern-matching on the resulting `DbError::{Configuration, ValidationFailed}`, and asserting `code == "x"`, `message == "y"`, `hint == Some("z".to_string())`.

  Verification:
  ```
  $ grep -n 'config_hinted\|validation_hinted' crates/plugin-db/src/error.rs
  299: /// use [`DbError::config_hinted`].
  311: pub fn config_hinted(
  333: pub fn validation_hinted(
  ```
  Constructors defined but no `#[test] fn config_hinted_*` / `#[test] fn validation_hinted_*` in `error.rs::tests`.

### [MEDIUM] crates/plugin-db/src/v8_classes/transaction.rs — 0 unit tests, 8+ uncovered no-DB branches

  Why: The Transaction wrapper has Drop, end(), and constructor logic that is independent of Postgres. Currently 100% of those branches rely on integration tests that drive a real BEGIN → COMMIT cycle. The cancellation / drop / settled-already / token-mismatch paths cannot be reached from `auto_tx.rs` because that suite always settles cleanly. Some behaviour-critical branches (the ROLLBACK-clears-pending-emits at line 266-270) are not directly observable from outside.

  Fix: Add a `#[cfg(test)] mod tests` to `transaction.rs` with the lightweight cases enumerated in §3.2:
  - "Illegal constructor" via the v8 scaffold from `tests/db_v8_class.rs:143`
  - `tx.collection("")` rejects with the expected TypeError message
  - `tx.collection(name)` cache identity (two calls return same object)
  - `Drop` no-op when `token == 0` or `settled == true`
  - `Drop` no-op when `token != current_tx_token`
  - `end()` no-op when `settled == true` (idempotent commit/rollback contract)

  Verification:
  ```
  $ grep -cE '#\[test\]|#\[compio::test\]' crates/plugin-db/src/v8_classes/transaction.rs
  0
  $ grep -n 'Illegal constructor' crates/plugin-db/tests/*.rs
  (no matches)
  ```

### [MEDIUM] crates/plugin-db/src/v8_classes/collection.rs — 0 unit tests, 11 type-error message branches uncovered

  Why: `upsert` and `findOrCreate` each carry a 5-branch `match conflict_v` (Null/String/Number/Bool/Object) that produces a typed error message naming the offending type (`crates/plugin-db/src/v8_classes/collection.rs:216-227, 263-274`), plus a separate empty-array branch (line 229-232, 276-279), plus `distinct`'s field validation (line 308-312). These messages are SDK-facing contracts (the SDK matches the wording in places). No test asserts them.

  Fix: Add a v8-scaffold integration test that:
  - For each `opts.conflictFields` shape (null/string/number/bool/object), calls `upsert` / `findOrCreate` and asserts the rejection message contains "got string" / "got number" / etc.
  - Calls `distinct(filter, {})` and asserts the "opts.field must be a non-empty string" error.

  Verification:
  ```
  $ grep -n 'opts.conflictFields must be' crates/plugin-db/tests/*.rs
  (no matches)
  $ grep -cE '#\[test\]|#\[compio::test\]' crates/plugin-db/src/v8_classes/collection.rs
  0
  ```

### [MEDIUM] crates/plugin-db/src/backend/postgres.rs::create_index_with_recovery_audited — 5+ classification branches uncovered (carry-over from r5/r6/r7/r8)

  Why: Same finding repeated since r5. The SQLSTATE classification table (fatal vs transient vs unknown) is pure logic that the worker depends on for index-build correctness. A future SQLSTATE addition (e.g., promoting `EXCLUSION_VIOLATION` to fatal) would land with zero test signal. Branch table in §3.1.

  Fix: Either extract the classification into a free function `classify_cic_failure(code: Option<&SqlState>) -> CicOutcome` and unit-test it, or land a `cargo test --test integration` fault-injection harness that pre-poisons the table to trigger UniqueViolation (e.g., insert two duplicate rows then deploy a unique index) and asserts the SchemaRefused envelope's `code` / `sqlstate` / `attempts` fields.

  Verification:
  ```
  $ grep -rn create_index_with_recovery_audited crates/plugin-db/
  src/backend/postgres.rs:356, 385  (production only)
  $ grep -n cic_failed crates/plugin-db/tests/*.rs
  (no matches)
  ```

### [LOW] crates/plugin-db/src/crud.rs — 0 unit tests on the dispatch_op template and resolvers

  Why: `run_op` (line 60-93), `first_row_or_null` (106), `row_count_as_f64` (115), `rows_as_json_array` (124) are pure functions. `first_row_or_null` has the most behaviour: empty vec → `null`, non-empty vec → first element. Trivially unit-testable, currently 0 unit tests.

  Fix: Add `#[cfg(test)] mod tests` in `crud.rs` with:
  - `first_row_or_null_empty_returns_null` / `first_row_or_null_returns_first_row`
  - `row_count_as_f64_extracts_count_column` / `row_count_as_f64_missing_column_yields_zero`
  - `rows_as_json_array_preserves_order`

  Verification:
  ```
  $ grep -cE '#\[test\]|#\[compio::test\]' crates/plugin-db/src/crud.rs
  0
  ```

### [LOW] crates/plugin-db/src/v8_bridge.rs::column_to_json — 13 OID branches + 4 overflow branches, 0 unit tests

  Why: Every Postgres `Row` returning to JS routes through this match. The overflow-checked TIMESTAMP / DATE branches (line 416, 438-440) explicitly handle the "Postgres ±infinity" case by returning `Value::Null` instead of panicking. That panic-guard is the kind of thing that *needs* a test because it isn't reachable from any natural workload — it requires constructing a `raw_value` with `i64::MAX` bytes. Today: 0 unit tests in `v8_bridge.rs`.

  Fix: Extract `column_to_json` to take `(oid: u32, raw: Option<&[u8]>, name: &str, row_get: impl Fn(...))` (or similar) so it's unit-testable without a real Row, then add cases for each OID + the two overflow branches.

  Note: This is a `[LOW]` rather than `[MEDIUM]` because the overflow-to-Null behaviour is defensive — a regression to a panic would be caught by the worker thread crashing on `i64::MAX` data, which is unlikely to occur in production. But the explicit invariant deserves a pin.

  Verification:
  ```
  $ grep -cE '#\[test\]|#\[compio::test\]' crates/plugin-db/src/v8_bridge.rs
  0
  ```

### [INFO] crates/plugin-db/src/orchestrator/lock_guard.rs:168-179 — unlock-SQL error branch (warn!) uncovered

  Why: r8 asked whether an integration-test path was needed. **No new integration test is needed**, because the happy-path unlock SQL *is* exercised by `tests/integration.rs:1382-1402`. The remaining branch is the `tracing::warn!` on unlock failure, which needs fault injection to reach. Logging-only branch with no return-value implication; documented in source.

  Verification:
  ```
  $ grep -n 'pg_advisory_unlock failed' crates/plugin-db/
  src/orchestrator/lock_guard.rs:176 (production only)
  ```
  Not actionable in current backend abstraction.

---

## Strengths

- **f1c5184e's hint test is well-targeted**: behaviourally pins the *new* invariant (Configuration carries `Some(_)`) at the exact boundary that introduced it. Good discipline.
- **The three test-neutral commits (3d79d2da, 09e32998+bc4363f0, 9e392ba1) are correctly test-neutral**: docs-only / surface-only / preamble-only changes shouldn't ship phantom tests. The previous rounds' feedback about not shipping "refactor without tests" is being respected for the right kind of refactor.
- **Lib suite hygiene maintained**: 371 passing, 0 failed, 0 ignored, 0 measured. Same fast (<200ms) execution. No new flakes.
- **lock_guard integration path is solid**: r8's open question resolves cleanly. `tests/integration.rs:1382-1402` already exercises the live unlock SQL via `pg_try_advisory_lock`.
- **`r8 carry-over for lock_guard 348-382` (structural source-text invariant test) continues to pay off**: it's the cancellation-safety pin that compiles into the build, not a runtime check.

---

## Score (1-100)

**84 / 100** (r8 was 84; r7 was 83)

Delta vs r8: **+0**

Reasoning:
- **+1** for the well-scoped f1c5184e hint test that pins the new invariant at the exact site that introduced it. This is the right shape of test for a wire-contract change.
- **-1** for the new sibling-hint-site (`replication.rs:282 wal_level_not_logical` hint string) shipping unasserted. The pattern of "introduce a cross-cutting contract and only test one of the N sites" is exactly the kind of partial-coverage concern raised in r7/r8 about classify_p0001_detail. The hint constructors (`config_hinted`, `validation_hinted`) also lack direct tests.
- **±0** holding pressure on the long-tail gaps: bare-files cluster (4 modules, 1,815 LOC, 0 unit tests) and `create_index_with_recovery_audited` (5+ branches) are unchanged since r6. Not new debt, but not resolved either. r9 doesn't widen these gaps but doesn't narrow them.
- **±0** for the three test-neutral commits being correctly test-neutral.

Net: the +1 from the hint test and the -1 from the sibling-site partial-coverage cancel. The bare-files and CIC gaps remain — they are persistent technical debt that hasn't moved since r6. Score holds.

## Comparison summary

```
            Lib   Integ  Major gaps closed         Major gaps new            Score
r6 (anchor) 343   73+1   —                         —                         82
r7          358   73+1   I40 structural            I42, guard                83
r8          364   73+1   I42 structural,           classify_p0001            84
                         guard ×4,
                         WalConsumer split
r9          371   73+1   lock_guard release        wal_level hint unasserted, 84
                         path (resolved via         config_hinted untested
                         existing integ test)
```

