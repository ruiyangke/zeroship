# Plugin-db Test Coverage Audit — Round 8 (2026-05-22)

Fresh re-audit. Prior anchor: r7 (83/100). Cycle context: r6 (82) → r7 (83).

## Tool baseline

```
cargo test -p zeroship-plugin-db --lib
  → 364 passed; 0 failed; 0 ignored; 0 measured (was r7: 358)
cargo test -p zeroship-plugin-db --features test-helpers --tests -- --list
  → integration:           73 tests   (unchanged)
  → integration #[ignore]:  1 test    (unchanged — sole entry at integration.rs:4175)
  → auto_tx:                8 tests   (DB-required; fail without postgres)
  → db_v8_class:            4 tests   (pass)
  → capability:             3 tests   (pass)
  → subscription_finalizer: 2 tests   (pass)
```

`auto_tx` fails locally because no `DATABASE_URL` / postgres socket is reachable in this sandbox. CI runs it against a real Postgres; these are intentional DB-requiring tests, not flakes.

Lib delta vs r7: **+6 tests** (358 → 364) — matches the 5 ConsumerRunningGuard / I42-structural additions in 386f9bf5 plus 1 more from 70921112 / aa639715 follow-ups.

## Per-file lib breakdown (lib.rs `#[test]` blocks)

```
query.rs                       147
context.rs                      34
broker.rs                       29
wal_consumer.rs                 26       (+3 vs r7: new() result tests)
read_set.rs                     20
replication.rs                  14
error.rs                        14
diff.rs                         11
v8_classes/replication.rs       10
auth/session.rs                  9
v8_classes/migration.rs          8
orchestrator/lock_guard.rs       6       (+1: release_flips_flag_after_unlock_await_structural)
replication_ops.rs               4       (NEW file-level tests — ConsumerRunningGuard lifecycle ×4)
orchestrator/auto_tx.rs          4
auth/keys.rs                     4
auth/bootstrap.rs                4
audit.rs                         4
v8_classes/db.rs                 3       (resolve_consumer_app_id tests; the v8_class itself is opaque)
v8_classes/subscription.rs       2
orchestrator/register_model/apply.rs    3
migrations.rs                    3
exec.rs                          3
backend/postgres.rs              2       (compile-time bound assertions)
backend/mod.rs                   2

— ZERO #[test] —
crud.rs                          0   (584 LOC — every dispatch_* is bare)
v8_bridge.rs                     0   (497 LOC — runtime_state, get_string_arg, v8_value_to_serde_json, fmt_db_err …)
v8_classes/collection.rs         0   (393 LOC — mint_collection, brand-check plumbing)
v8_classes/transaction.rs        0   (336 LOC — mint_transaction)
orchestrator/transaction.rs      0   (173 LOC — begin_transaction_dispatch)
orchestrator/register_model/{validate,plan,mod,bootstrap}.rs  0
```

## R7 carry-over status

| r7 gap | r8 status | Evidence |
|---|---|---|
| `create_index_with_recovery_audited` 5 branches (cic_failed envelope, fatal SQLSTATE class 23xxx, transient retry, invalid-index-landed, cic_configuration fallthrough) | **STILL UNCOVERED** at unit level. `cic_configuration` is unreachable-by-construction per the loop logic but still untested. | `grep cic_failed crates/plugin-db/tests/` → 0 hits; `grep cic_configuration` → 0 hits |
| `lock_guard.release()` actual SQL path (lines 160-180) | **PARTIALLY CLOSED**. 386f9bf5 added `release_flips_flag_after_unlock_await_structural` (source-text invariant). The real PG-error warn branch (lines 172-179) is still unexercised. | All 6 lock_guard tests use `for_test_no_client`; no integration test forces a unlock-SQL failure. |
| Unicode 63-byte boundary on `validate_collection` / `validate_field_name` | **STILL UNCOVERED**. Test at query.rs:4210 uses `"a".repeat(64)` only. | `grep -E "unicode\|emoji\|\\\\u\\{" crates/plugin-db/src/query.rs` → 0 hits |
| 4 bare files (crud.rs, v8_bridge.rs, v8_classes/collection.rs, v8_classes/transaction.rs) | **STILL bare** — now 1810 LOC across the 4 (584+497+393+336) + 173 in orchestrator/transaction.rs that r7 didn't flag. | grep above |

## Recent-commit coverage (the four called-out commits)

| Commit | Behaviour | Direct unit test? | Indirect? |
|---|---|---|---|
| 386f9bf5 | `release_flips_flag_after_unlock_await_structural` + 4 ConsumerRunningGuard lifecycle tests | **YES** ×5 — at lock_guard.rs:349 and replication_ops.rs:378-424. The try_claim test (414-424) catches the `then_some` → `then(\|\|)` regression by name. | n/a |
| a272d1af | `classify_p0001_detail` (5 DETAIL token branches) | **NO** — none. Function is private at session.rs:174 and takes a `compio_postgres::Error` that can't be fabricated without a live DB. | Integration `b8c_init_session_rejects_*` covers 3 of 5 tokens (`expired`, `replay`, `invalid_signature`) — **2 untested**: `session_invalid_actor_kind` and `session_nonce_too_short`. |
| aa639715 | `WalConsumer::new` returns typed `Result<_, DbError>` | **YES** — `wal_consumer_new_invalid_app_id_returns_typed_error` (line 949) + `wal_consumer_new_missing_db_url_returns_configuration` (line 972) pin both legs of the Configuration-vs-ValidationFailed split. | n/a |
| 70921112 | `try_mark_consumer_running` atomic check-and-set | **NO direct test** at the `IsolateDbContext::try_mark_consumer_running` level (only `mark_consumer_running` is tested at context.rs:893-915). | **YES** — `consumer_running_guard_try_claim_loses_when_already_marked` (replication_ops.rs:413) exercises the atomic semantics through the guard. |

`mark_consumer_running` gating sanity-check: confirmed `#[cfg(any(test, feature = "test-helpers"))]` at context.rs:423. Production builds cannot call the non-atomic variant.

## Findings

```
[MEDIUM] crates/plugin-db/src/auth/session.rs:174-202 — Missing direct coverage of classify_p0001_detail
  Why: 5-branch match introduced in a272d1af. 2 of 5 DETAIL tokens
       (session_invalid_actor_kind, session_nonce_too_short) are not exercised
       by any integration test. Adding a 6th branch later wouldn't trip CI.
  Fix: lift classify_p0001_detail to take a minimal trait so a fake DbError
       with arbitrary detail() can be injected; add 5 direct unit tests + 1
       fallthrough (P0001 with unknown detail → None) + 1 negative
       (non-P0001 SQLSTATE → None). Or add integration tests that mint a
       token with empty actor_kind and a 15-byte nonce.
  Verification: grep -rn "session_invalid_actor_kind\|session_nonce_too_short"
       crates/plugin-db/tests/ → 0 hits.

[MEDIUM] crates/plugin-db/src/backend/postgres.rs:385-600 — Missing coverage of create_index_with_recovery_audited
  Why: 5 distinct terminal branches (cic_failed via invalid-index-landed,
       fatal SQLSTATE 23xxx, transient-then-give-up at MAX_RETRIES,
       invalid_index_landed at MAX_RETRIES, cic_configuration fallthrough).
       Carried since r6. The branch coverage is zero — no unit test in
       backend/postgres.rs::tests (the test module declares it explicitly:
       "no stub / no-IO constructor"), no integration test asserts the
       envelope JSON shape.
  Fix: extract the inner `refuse` envelope builder + branch classifier into
       a free function taking `Option<SqlState>` + attempt counter; unit-test
       each branch's envelope JSON; integration test would need a CIC induced
       failure (kill the connection mid-CREATE INDEX CONCURRENTLY).
  Verification: grep -nE "cic_failed\|cic_configuration\|invalid_index_landed"
       crates/plugin-db/tests/ → 0 hits.

[LOW] crates/plugin-db/src/orchestrator/lock_guard.rs:172-179 — Missing coverage of pg_advisory_unlock warn branch
  Why: ffb1e101 added the warn-on-error guard. Regressing to `let _ =`
       passes every existing test. r7 already flagged; still open.
  Fix: integration test that kills the PG session between lock acquisition
       and release(); assert tracing warn event captured by a test layer.
  Verification: grep -rn "pg_advisory_unlock failed" crates/plugin-db/tests/
       → 0 hits.

[LOW] crates/plugin-db/src/migrations.rs:638-651 — Missing coverage of finalise_backfill warn path
  Why: 51ced4a0 added the warn-on-error path. Same shape as above. Carried
       from r7.
  Fix: tracing-test layer + a mock Backend whose finalise_backfill returns
       Err; assert warn event with audit_id field.
  Verification: grep -rn "finalise_backfill failed" crates/plugin-db/tests/
       → 0 hits.

[LOW] crates/plugin-db/src/query.rs:61-99 — Unicode 63-byte boundary uncovered
  Why: validate_collection checks `name.len() > 63` (byte length) before the
       ASCII filter. A 63-byte name composed of multibyte UTF-8 (e.g. 21 ×
       3-byte chars = 63 bytes) would fall through the byte check and only
       be rejected by the ASCII filter. The current test uses `"a".repeat`
       so the byte/char distinction is invisible. If the ASCII filter is
       ever relaxed (e.g. to permit Unicode identifiers), the 63-byte branch
       could silently accept multibyte names that Postgres truncates.
  Fix: validate_collection_rejects_multibyte_at_63_byte_boundary using a
       63-byte 3-byte-per-char string. Likewise for validate_field_name.
  Verification: grep -nE "validate_collection.*\\\\u\\{\|multibyte\|emoji"
       crates/plugin-db/src/query.rs → 0 hits.

[LOW] crates/plugin-db/src/crud.rs (584 LOC) — Bare module
  Why: 13 dispatch_* functions (find/find_one/insert/insert_many/update_one/
       update_many/delete_one/delete_many/aggregate/distinct/count/upsert/
       find_or_create), zero #[test]. Coverage relies entirely on
       integration.rs DB-bound tests. The non-DB validation pre-checks
       (capability gating, JSON arg shape, dispatch table) could be unit
       tested at lower cost.
  Fix: extract the pure pre-dispatch validation into helpers that take
       SerdeValue + capability flags; unit test each helper.
  Verification: grep -c "#\[test\]" crates/plugin-db/src/crud.rs → 0.

[LOW] crates/plugin-db/src/v8_bridge.rs (497 LOC) — Bare module
  Why: get_string_arg, get_i64_arg, v8_value_to_serde_json, read_json_arg,
       fmt_db_err, rows_to_json_value, row_to_json — all called from every
       dispatch path, none directly tested. v8_value_to_serde_json
       especially carries non-trivial recursion (object→Map, array→Vec,
       bigint→i64) that's exercised only indirectly.
  Fix: v8::OwnedIsolate-based test fixtures (the runtime crate's tests
       module already has the pattern); 10-15 tests for the json
       conversions + 2-3 for fmt_db_err shape.
  Verification: grep -c "#\[test\]" crates/plugin-db/src/v8_bridge.rs → 0.

[LOW] crates/plugin-db/src/v8_classes/collection.rs (393 LOC) — Bare module
  Why: mint_collection brand-check + Weak finalizer plumbing untested.
  Fix: same pattern as v8_classes/db.rs::tests — drive `mint_collection`
       through a stub isolate; assert brand identity + GC reclamation.
  Verification: grep -c "#\[test\]" crates/plugin-db/src/v8_classes/collection.rs → 0.

[LOW] crates/plugin-db/src/v8_classes/transaction.rs (336 LOC) — Bare module
  Why: mint_transaction has the same shape as mint_subscription (which
       got a structural V8-OOM test in r6); still untested here.
  Fix: source-text structural invariant mirroring
       mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure.
  Verification: grep -c "#\[test\]" crates/plugin-db/src/v8_classes/transaction.rs → 0.

[LOW] crates/plugin-db/src/orchestrator/transaction.rs (173 LOC) — Bare module
  Why: begin_transaction_dispatch — entry point for env.db.beginTransaction.
       Pure dispatch logic with no V8 plumbing in the entry function should
       be unit-testable.
  Fix: extract the pre-promise validation into a free function; test it.
  Verification: grep -c "#\[test\]" crates/plugin-db/src/orchestrator/transaction.rs → 0.
```

## Strong-coverage callouts (what's good)

- **R7-NEW MEDIUM closed**: `replication_ops.rs` now has 4 dedicated ConsumerRunningGuard lifecycle tests (replication_ops.rs:378-424). The `try_claim` test that pins the `then_some` → `then(\|\|)` regression is exactly the kind of behaviour-pinning test r7 asked for. **One of these tests caught a real latent bug.**
- **R7-HIGH partially closed**: `release_flips_flag_after_unlock_await_structural` (lock_guard.rs:349-382) is the source-text invariant test r7 asked for. The byte-offset pattern mirrors `mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure` — proven pattern, well-applied.
- **WalConsumer::new** typed-error split (aa639715) has clean unit tests for *both* legs of the MAJOR-R5-4 Configuration-vs-ValidationFailed split at wal_consumer.rs:949 and 972.
- **Type-level signature guards** at auth/session.rs:478-526 stop a future flattening-back-to-String regression at compile time. Good defensive pattern.
- Lib test count up to 364, all clean, no ignored, no flakes (same hygiene as r7).

## Score (1-100)

**84 / 100**  (r7 was 83; r6 was 82)

Delta vs r7: **+1**

Reasoning:
- **+2** for closing r7's NEW MEDIUM (ConsumerRunningGuard lifecycle ×4 tests, with one catching a real bug — that's the highest-value test type) and r7's HIGH at lock_guard.rs:160-183 via the structural invariant test. Two of the most concerning carry-overs were addressed.
- **+1** for the WalConsumer::new typed-result split being fully unit-covered (both legs of the MAJOR-R5-4 split pinned).
- **-1** for a272d1af shipping classify_p0001_detail with no direct unit tests AND 2 of 5 DETAIL branches uncovered even indirectly (session_invalid_actor_kind, session_nonce_too_short). The pattern of "refactor that improves precision but ships without tests for the new code paths" is exactly what r7 flagged about ffb1e101 / 51ced4a0.
- **-1** holding pressure: bare-files cluster (5 modules, ~1983 LOC) and create_index_with_recovery_audited's 5 branches remain untouched since r6. These are the long tail; not new debt but persistent.

Net: a real, evidence-backed +1. The codebase is in a slightly better place than r7, driven primarily by the ConsumerRunningGuard work which substantively improved the quality of the test suite (caught a bug). Not a leap because the same two long-running gaps (CIC branches, bare files) remain and a new mid-severity gap (classify_p0001_detail) opened.

## Comparison summary

```
            Lib   Integ  Major gaps closed  Major gaps new  Score
r6 (anchor) 343   73+1   —                  —               82
r7          358   73+1   I40 structural     I42, guard      83
r8          364   73+1   I42 structural,    classify_p0001  84
                         guard ×4,
                         WalConsumer split
```
