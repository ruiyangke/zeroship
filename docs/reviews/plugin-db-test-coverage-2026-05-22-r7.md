# Plugin-db Test Coverage Audit — Round 7 (2026-05-22)

## Lib test count + breakdown
- **358 passed** (+15 from r6's 343). All clean, no ignored, no flakes.
- New additions: `error.rs` +3 (first_row_or_internal × 2 + prefix_message × 2), `v8_classes/replication.rs` +4 (watchdog_app_id + drop_abandoned_app_id resolver tests), `replication.rs` +2 (watchdog_query_filters_by_app_id, drop_abandoned_slots_filters_by_app_id), `lock_guard.rs` +5 (RAII tests, all using `for_test_no_client`).

## Integration suite stability
- **73 active + 1 #[ignore]** in `tests/integration.rs`. Unchanged vs r6.
- No new integration coverage shipped for the 7 reviewed commits.

## R6 carry-over status

| r6 gap | Status r7 | Evidence |
|---|---|---|
| `create_index_with_recovery_audited` 5 branches | **Still UNCOVERED** | `grep cic_failed crates/plugin-db/tests/` returns no hits |
| `lock_guard.release()` SQL path | **Still UNCOVERED at unit level**. All 5 tests use `for_test_no_client` | Branch only reachable when `client = Some(_)` |
| [I40] V8-alloc-failure structural test | **CLOSED** | subscription.rs:288 |
| Unicode 63-byte boundary | **Still UNCOVERED**. Only ASCII tested. | query.rs:4210 uses `"a".repeat()` only |
| 4 bare files (crud.rs, v8_bridge.rs, v8_classes/collection.rs, transaction.rs) | **Still bare** — 1810 LOC total | Zero `#[test]` blocks |

## Critical findings

```
[HIGH] crates/plugin-db/src/orchestrator/lock_guard.rs:160-183 — Missing coverage of [I42] cancellation safety
  Why: bd1e7ce1's defining behaviour (released=true flip AFTER unlock-SQL await) is
       not pinned by any test. Could be silently reverted.
  Fix: integration test that drops the future mid-await, asserts the Drop log fired.
       Or a source-text invariant test (analogous to mint_subscription_does_not_leak).
  Verification: grep returns no matches.
```

```
[MEDIUM] crates/plugin-db/src/replication_ops.rs:277-294 — Missing coverage of ConsumerRunningGuard mark/unmark lifecycle
  Why: 34d209b5 + e399eeea touched both new() and Drop, but guard coupling itself
       is not directly tested. Only the primitives in context.rs are tested.
  Fix: lift ConsumerRunningGuard out of local function scope, add 3 tests:
       guard_new_marks, guard_drop_unmarks, guard_drop_unmarks_on_panic_unwind.
  Verification: no test references to ConsumerRunningGuard.
```

```
[LOW] crates/plugin-db/src/migrations.rs:644-657 — Missing coverage of finalise_backfill warn path
  Why: regressing to `let _ =` would not fail any test.
  Fix: tracing-test layer + induced backend failure + assert warn event.
  Verification: grep "finalise_backfill" in tests/ returns no hits.
```

```
[LOW] crates/plugin-db/src/orchestrator/lock_guard.rs:168-179 — Missing coverage of ffb1e101 pg_advisory_unlock warn path
  Why: same shape as finalise_backfill — silently revertible with no test failure.
  Fix: integration test that issues release().await against a killed PG session.
  Verification: grep "pg_advisory_unlock failed" in tests/ returns no hits.
```

## Strong-coverage callouts

- `error.rs` consolidation: 12 dedicated tests cover `prefix_message`, `coded_sql`, `first_row_or_internal` — textbook contract test discipline.
- `v8_classes/replication.rs` cross-app scoping: 10 resolver tests + 2 SQL-binding tests pin both layers.
- `mint_subscription` source-text structural invariant test (subscription.rs:288) — creative pattern for hard-to-trigger V8-OOM conditions.

## Score: 83/100

vs r6: 82 → 83 (**+1**)

Drivers:
- +1.5 [I40] V8-alloc structural test landed
- +1.0 error.rs consolidation tests exemplary
- +0.5 c0590506 cross-app scoping well-defended
- -1.0 [I42] release-flag ordering still unverified (second-round carry-over)
- -0.5 ConsumerRunningGuard coupling lacks direct test
- -0.5 Two warn-path commits (ffb1e101, 51ced4a0) shipped with no behavioural test
