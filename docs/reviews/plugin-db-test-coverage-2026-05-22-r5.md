# Plugin-db Test Coverage Audit — Round 5 (2026-05-22)

## Tool Results

- `cargo test -p zeroship-plugin-db --lib`: **336 passed, 0 failed, 0 ignored** in 0.15s
- `cargo test -p zeroship-plugin-db --features test-helpers --test integration -- --list`: **73 tests** (1 still `#[ignore]` — `p8a2_supervised_consumer_exits_on_slot_invalidated`)
- Per-file test count (top 13): 147 query.rs, 34 context.rs, 29 broker.rs, 25 wal_consumer.rs, 20 read_set.rs, 11 diff.rs, 10 error.rs, 9 replication.rs, 8 v8_classes/migration.rs, 8 auth/session.rs, 5 orchestrator/lock_guard.rs, 4 v8_classes/replication.rs, 4 orchestrator/auto_tx.rs.

## 1. Recent-commit Test Discipline

### `cbd12944` — OrchestratorLockGuard (5 tests in `lock_guard.rs`)

Five tests, all using a `for_test_no_client` helper that bypasses `acquire()`:

1. **`release_idempotent_when_no_client`** — drives a `Runtime::new().block_on(guard.release())` on a `client: None` guard; asserts `Ok(None)` and no panic.
2. **`into_held_flips_released_flag`** — does NOT actually call `into_held()` (would panic on `.expect()` with no client). Instead it manually flips `released = true` and drops — **only the prefix of the function under test runs**. The `.expect("…called on guard with no client")` branch and the actual client hand-off are unobserved.
3. **`drop_with_released_true_does_not_warn`** — flips `released = true` then drops; asserts no panic. Cannot capture tracing output.
4. **`drop_with_released_false_runs_warning_branch`** — drops un-released guard; "tracing::error path doesn't panic" smoke test. Cannot capture log line.
5. **`released_flag_starts_false`** — constructor field-check.

**Verdict — partial.** They genuinely pin the flag-state transitions (the bug class `released` was added to guard against), and #4 explicitly hits the Drop warning branch. Gaps:
- The real `release().await` path (sending the unlock SQL) is unreachable in unit tests because `PooledClient` cannot be constructed without a live pool — no test exercises the SQL.
- The actual `acquire()` → `release()` round-trip is integration-only and **not currently covered by any integration test** I can find (no test grabs the guard and observes its release).
- `into_held()` is `#[allow(dead_code)]` AND not exercised by either unit or integration tests.

### `8ff1b2de` — auto_tx error rail (4 tests in `auto_tx.rs`)

1. **`auto_begin_transient_error_preserves_code`** — feeds `DbError::Transient { … }` into `begin_to_resolve_value`; asserts `code == "transient"` and `hint.is_some()`.
2. **`auto_end_lock_contention_preserves_code`** — feeds `DbError::LockContention { … }` into `end_to_resolve_value`; asserts `code == "lock_not_available"`.
3. **`auto_begin_ok_resolves_with_token`** — `Ok(1)` → `ResolveValue::U32(1)`.
4. **`auto_end_ok_resolves_with_undefined`** — `Ok(())` → `ResolveValue::Undefined`.

**Verdict — solid for the diagnosed bug.** Both `.code` preservations are asserted, plus both success paths (sanity guard against Ok/Err arm-swap regressions). Gaps:
- `DbError::Internal`, `Configuration`, `ValidationFailed`, `SchemaRefused`, `UniqueViolation` — none of the other variants is tested at this boundary; if `to_op_error()` changes their `code` they pass silently.
- The actual COMMIT/ROLLBACK paths in `exec_auto_end` (lines 257–274) are integration-only (in `tests/auto_tx.rs`).
- `normalize_isolation` (lines 157–176) has zero unit tests — no test pins `read uncommitted` → `READ COMMITTED` alias, empty-string fallback, or whitespace-collapse.

### `0e58c4e8` — broker two-level HashMap (9 tests in `broker.rs`)

1. `has_subscribers_lifecycle` — pre/post subscribe/close/publish.
2. `has_subscribers_false_when_app_unknown` — explicit negative.
3. `has_subscribers_false_when_collection_unknown` — same app diff collection.
4. `has_subscribers_true_for_registered_pair` — positive.
5. `has_subscribers_false_after_publish_prunes_closed` — close + publish then probe.
6. `has_subscribers_takes_str_no_string_alloc_at_call_site` — compile-time `&str` signature guard.
7. `has_subscribers_isolated_across_apps_and_collections` — cross-product.
8. `publish_drops_per_app_map_when_last_collection_empties` — whitebox check on `by_key.len() == 0` after last collection.
9. `drop_app_closes_all_subscribers` — fan-out drop_app.

**Verdict — excellent.** Every observable behaviour from the two-level rewrite is pinned: app-only miss, collection-only miss, alloc-free probe shape, lazy GC of dead buckets, drop_app fan-out. The compile-time `&str` test is the right shape to catch a signature regression. **This is the strongest test set of the four commits.**

### `c83d6a8c` — replication empty-RETURNING (2 tests in `replication.rs`)

1. **`ensure_publication_and_slot_empty_returning_is_internal_error`** — value-level: rebuilds the exact `ok_or_else` closure and asserts `DbError::Internal { message: "replication: pg_create_logical_replication_slot returned no row" }`.
2. **`empty_returning_string_shape_keeps_replication_prefix`** — checks `into_string()` output retains `"replication:"` prefix, `"pg_create_logical_replication_slot"` op tag, and `"no row"`.

**Verdict — surgical and correct.** The bug was specifically `.unwrap_or_default()` silently producing `lsn = ""`; the test pins the error message shape AND the operator-facing string format. The closure is rebuilt verbatim, so a future rename of either string is caught. The integration coverage is `c1_setup_creates_publication_and_slot_idempotently` (line 2856 of integration.rs) which exercises the success path with a live Postgres — both paths covered.

## 2. Lib Test Count

**336 passed / 0 failed / 0 ignored.** This is the new baseline.
- Top heavy files: query.rs (147), context.rs (34), broker.rs (29), wal_consumer.rs (25).

## 3. Integration Suite Status

- **73 listed tests**.
- **1 `#[ignore]`d**: `p8a2_supervised_consumer_exits_on_slot_invalidated` at line 4146 (driver can't surface SQLSTATE 58P01 — documented in the comment, blocked on driver work, not a flaky test).
- **No hang** observable in the listing pass.

## 4. Branches Still Uncovered

### `create_index_with_recovery_audited` (R3 carry — 6 terminal branches)

```
[HIGH] crates/plugin-db/src/backend/postgres.rs:387–602 — create_index_with_recovery_audited still has 5 uncovered terminal branches
  Why: regression risk on a 215-line function whose only happy-path is covered.
  Fix: 5 unit tests against a mocked Backend stub.
```

### OrchestratorLockGuard's actual-on-PG-error release path

```
[MEDIUM] crates/plugin-db/src/orchestrator/lock_guard.rs:114–129 — release() SQL path never executed in tests
  Why: the call to the database — the only operationally-relevant code — is never tested.
  Fix: integration test that drives acquire(), forces an error, probes pg_locks for release.
```

## 5. Edge Cases STILL Not Covered

```
[MEDIUM] crates/plugin-db/src/query.rs:117 — Unicode boundary on byte-length check (R4 gap, not fixed)
  Why: validator uses name.len() (bytes). Tests only use ASCII.
  Fix: parametric test with multi-byte chars at byte boundaries.

[MEDIUM] (no test file) — mid-stream connection drop in subscribe AsyncIterable not covered
  Why: regression risk on dangling broker subscriptions when WS/SSE peer drops mid-iteration.

[HIGH] crates/plugin-db/tests/integration.rs:1322 — concurrent registerModel is sequential, not concurrent
  Why: the test explicitly admits it doesn't exercise concurrency. Advisory-lock contention contract unverified.
  Fix: spawn two exec_register_model_with_pool futures with overlapping start times.
```

## 6. Files Still Bare (R4 Carry)

| File | LOC | `#[test]` count | Status |
| --- | ---: | ---: | --- |
| `crates/plugin-db/src/crud.rs` | 584 | **0** | UNCHANGED |
| `crates/plugin-db/src/v8_bridge.rs` | 497 | **0** | UNCHANGED |
| `crates/plugin-db/src/v8_classes/collection.rs` | 393 | **0** | UNCHANGED |
| `crates/plugin-db/src/v8_classes/transaction.rs` | 336 | **0** | UNCHANGED |

## Summary

**Strengths since R4 (84):**
- broker tests are exemplary
- replication empty-RETURNING tests are surgical
- auto_tx tests correctly assert .code preservation for the two most retry-critical variants
- Lib test count 330→336

**Weaknesses (carry from R4, mostly unmoved):**
- lock_guard tests pin flag state but never observe actual unlock SQL
- auto_tx tests only cover 2 of ~8 DbError variants at conversion boundary
- create_index_with_recovery_audited still has 5 uncovered terminal branches
- Concurrent registerModel test is sequential
- Unicode 63-byte boundary STILL not added
- Mid-stream connection drop not covered
- All 4 R4-flagged bare files (1810 LOC combined) at zero unit tests

**Score: 81 / 100** (R4 was 84). Net -3:
- −2 for lock_guard tests not exercising the actual unlock SQL (smoke-quality)
- −2 for auto_tx tests only covering 2 of ~8 DbError variants
- +1 for the broker commit setting a new bar in unit-test discipline
