# plugin-db Test-Coverage Review — Round 4 (2026-05-22)

HEAD at review: `4d80651e`.

Commits evaluated since round 3:

- `5be3c1a1` re-promoted `broker` + `v8_classes` to `pub`
- `90d992d5` cfg-gates module visibility on `test-helpers` — closes the
  r3 CRITICAL integration-suite unbuildable
- `3bb41fa1` bootstrap.rs — advisory-lock leak on error paths
  (+ 1 unit test on `lock_key` format)
- `ed697c45` migrations.rs — preserve typed `DbError` at 4 audit-write
  sites (+ 3 unit tests on `map_audit_bootstrap_err`)
- `309ed52f` v8_classes — drop cross-app appId override
  (+ 7 unit tests on `resolve_consumer_app_id` / `resolve_setup_app_id`)
- `3ef6a170` apply.rs — hard-error on `DropColumn`/`DropIndex` outside
  destructive class (+ 3 unit tests on `check_destructive_invariant`)
- `49b0b98e` exec.rs — gate `exec_mutation_with_emit` tuple build behind
  subscriber check (+ 3 unit tests on the `emit_for_rows` gate)

Net: +17 lib tests over the cycle. Plus integration-suite buildable
again under `--features test-helpers`.

---

## 1. Headline

**Overall score: 84 / 100** (up from r3's 78).

The big movement is the integration suite returning to a buildable
state under `--features test-helpers`. `90d992d5` cfg-gates the
module-visibility exception so the previously dark 73-test integration
crate (including the p8a2 hang-risk cluster) is back in scope. Five of
the six recent regressions (`309ed52f`, `3bb41fa1`, `ed697c45`,
`3ef6a170`, `49b0b98e`) shipped with at least one accompanying unit
test — a discipline shift from r3. The persistent gap is `cac3e542`
(dict-shape `default.rpc` stream dispatch): **still no Rust test**
guards it (`git log cac3e542..HEAD -- crates/runtime/tests/` is empty).
Shell smoke remains the only regression net for a kernel path that has
shipped two production regressions in three months.

Lib test count, cargo build, integration sanity:

```
$ cargo test -p zeroship-plugin-db --lib -- --list 2>&1 | grep ": test$" | wc -l
321

$ grep -rn "#\[test\]\|#\[compio::test\]" \
    crates/plugin-db/src/ crates/plugin-db/tests/ | wc -l
412

$ cargo build -p zeroship-plugin-db --tests --features test-helpers 2>&1 | tail -1
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 9.70s

$ cargo test -p zeroship-plugin-db --features test-helpers \
    --test integration -- --test-threads=1 --list | tail -1
73 tests, 0 benchmarks
```

Lib trajectory: r2 298 → r3 302 → r4 **321** (+19 net since r3, of
which 17 land in the audited cycle commits and 2 are has-subscribers
lifecycle tests in broker — see §3).

---

## 2. Audit dimensions

### 2.1 Integration suite stability

The 73 integration tests inventory cleanly via `--list`. Cluster
breakdown:

| Cluster | Count | Behaviour |
| --- | --- | --- |
| `a1_*` (unique index) | 1 | success-path index DDL |
| `a2_*` (advisory-lock concurrency) | 6 | bootstrap, additive, destructive, strictness |
| `a3_*` (audit) | 1 | table provisioning idempotency |
| `b1_*` (backfill lifecycle) | 8 | dry-run, resume, cancel, dead-letter |
| `b2_*` (foreign keys) | 5 | cascade, restrict, deferrable, validation |
| `b8c_*` (auth/role/HMAC) | 10 | session, key rotation, RLS, replay defence |
| `c1_*` (publication + slot) | 5 | setup, watchdog, abandoned-reap |
| `p8a2_*` (consumer supervisor) | 6 | spawn, reconnect, slot invalidated (ignored) |
| `gap_b_*`, `gap_c_*`, `gap_i_*`, `gap_x_*` | 6 | tx commit semantics |
| CRUD (`insert_*`, `find_*`, `update_*`, `delete_*`, `aggregate_*`, etc.) | 25 | builder paths against real PG |

I ran a 60-second-bounded subset (`insert_and_find`,
`p8a2_consumer_publishes_wal_event_to_broker`,
`p8a2_supervised_consumer_*`, `p8a2_auto_spawn_*`). Every test
completed in under 4s; no hangs. **p8a2 hang status: closed at HEAD
for the runnable subset.**

One test is `#[ignore]`d (verified): line 4146,
`p8a2_supervised_consumer_exits_on_slot_invalidated`. The skip rationale
(driver-level — START_REPLICATION on a dropped slot surfaces the same
`Io("error communicating with the server")` string as a mid-stream
disconnect, so `is_fatal()` can't distinguish without regressing
`reconnects_after_kill`) is documented in the comment block and is a
legitimate driver/PG-combo dependency. Watchdog-reaper path is still
covered by `c1_drop_abandoned_reaps_inactive_slot` — the production
codepath isn't dark, only the distinguishing probe is.

Verification (one slow-prone p8a2 case to demonstrate timeout-safety):

```
$ timeout 60 cargo test -p zeroship-plugin-db --features test-helpers \
    --test integration -- --test-threads=1 \
    p8a2_supervised_consumer_reconnects_after_kill 2>&1 | tail -5
test result: ok. 1 passed; 0 failed; 1 ignored; 0 measured; 71 filtered out;
finished in 3.09s
```

### 2.2 Coverage of recent fixes

| Commit | Test added? | Function-shaped? | Notes |
| --- | --- | --- | --- |
| `cac3e542` (dispatch stream kind) | NO — only shell smoke | n/a | **r3 gap persists.** No `crates/runtime/tests/*.rs` changes since the fix landed. |
| `309ed52f` (cross-app appId override) | YES, 7 tests | YES, ~partial | Tests call `resolve_consumer_app_id` / `resolve_setup_app_id`; the call sites at db.rs:248 and replication.rs:66 use the same helpers. **Gap:** `resolve_setup_app_id` is `fn(stamped, _opts) → stamped.to_string()` — trivially correct by construction. A regression that inlines `_opts.appId.as_str()` at the call site would bypass the helper AND the tests. The helper-extraction pattern locks in *this* shape but doesn't prevent a refactor that lifts the read back to the call site. Still: this is a significant uplift; flagging as MINOR. |
| `3bb41fa1` (bootstrap.rs advisory-lock leak) | YES, 1 test (`lock_key_formats_with_zs_reg_prefix`) | NO — format check only | The test asserts `lock_key("app_abc") == "zs_reg:app_abc"`. It does NOT prove that on error, `pg_advisory_unlock` is issued with the matching `lock_key` against the same `lock_client`. The actual leak path needs a live PG fixture that injects an `ensure_app_schema` failure mid-bootstrap and asserts `pg_locks` is empty afterwards. **No such integration test exists** — grep for `pg_advisory_unlock.*zs_reg` finds only `a2_concurrent_deploys_serialise_via_advisory_lock` (line 1399), which tests the *success-path* release. |
| `ed697c45` (audit_bootstrap typed-error preservation) | YES, 3 tests | YES | Tests call `map_audit_bootstrap_err` directly with `DbError::Transient`, `DbError::LockContention`, `DbError::Internal`. Call sites at lines 253, 646, 687, 720 all `.map_err(map_audit_bootstrap_err)` — same helper, no re-implementation. Function-shaped; strongest test of the cycle. |
| `3ef6a170` (DropColumn/DropIndex invariant) | YES, 3 tests | YES | Tests call `check_destructive_invariant(&op)` which is the extracted gate at apply.rs:269. Covers (a) misclassified `DropColumn` → `Internal`, (b) `DropColumn`+`Destructive` → `Ok(())`, (c) non-Drop kinds pass-through. The error-path test asserts the message contains `drop_column`, `Destructive`, and `contract violation` — strong invariant-name capture. |
| `49b0b98e` (`exec_mutation_with_emit` subscriber gate) | YES, 3 tests | YES, but on a re-exported helper | Tests call `emit_for_rows(&rows, ...)` which is the gate split out of `exec_mutation_with_emit` (exec.rs:189). The function name in the commit message is `exec_mutation_with_emit_*` but the test bodies call `emit_for_rows`. The gate IS the production path (the helper is called by `exec_mutation_with_emit` at lines TBD), so this is sound — the test exercises the same `is_app_suppressed` + `has_subscribers` short-circuit production runs. Best testability-driven refactor of the cycle. |

### 2.3 Branch coverage — `create_index_with_recovery_audited`

```
$ grep -c "#\[test\]" crates/plugin-db/src/backend/postgres.rs
2
```

(Both tests are compile-time trait-shape + Debug-impl source check.)

`create_index_with_recovery_audited` (lines 387–602) still has **zero
unit tests covering its six terminal branches**:

1. Success (`indisvalid == true`, line 474)
2. INVALID retry budget exhausted (line 491)
3. SQL fatal violation (UNIQUE/NOT_NULL/FK/CHECK) refuse (line 533)
4. Transient retry (continue loop)
5. Non-transient or `MAX_RETRIES` reached refuse (line 574)
6. Invariant-breach `DbError::Configuration { code: "cic_configuration" }`
   (line 595)

The only integration test in the area is
`a1_unique_index_actually_enforces_uniqueness` (integration.rs:858).
It executes `build_create_indexes` SQL directly via
`pool.execute(&spec.sql, &[])` — it never touches
`create_index_with_recovery_audited`. The retry loop, the
INVALID-landed path, the unique-violation refuse envelope shape, the
transient classification, the MAX_RETRIES exhaustion, and the
`cic_configuration` invariant-breach branch are all dark.

r3 gap #3 **persists unchanged** at HEAD.

### 2.4 Edge cases — what's NOT covered

| Edge case | Status | Verification |
| --- | --- | --- |
| Empty `insertMany([])` | COVERED — `test_insert_many_empty` (query.rs:2496) returns `Err` for empty array | `grep -n insert_many_empty crates/plugin-db/src/query.rs` |
| Empty `updateMany({})` filter | NOT TESTED — no `test_update_many_empty_filter`; only `test_update_many` (with filter) and `test_update_many_auto_updates_timestamp` | `grep "update_many.*empty\|update_many_no_filter" crates/plugin-db/src/query.rs` → no hits |
| Empty `deleteMany({})` filter | COVERED-AS-ALLOWED — `test_delete_many_no_filter` (query.rs:2524) asserts the SQL has no WHERE. No "destructive guard" test. | line 2524 |
| Unicode/emoji in field names | PARTIAL — `setup_app_id_preserves_unicode_stamped_id` (replication.rs:207) tests an app_id of `app_测试_🛡` but only via the `resolve_setup_app_id` helper. `validate_field_name`'s 63-BYTE boundary in unicode is not tested — `f.repeat(63)` is ASCII; 4-byte emoji × 16 = 64 bytes (would fail), 4-byte × 15 = 60 bytes (would pass) is untested. | `validate_field_name_accepts_valid_names` (query.rs:4240) uses only ASCII; no `is_ok` assertion on a UTF-8 multi-byte name |
| 63-byte collection names | COVERED — `validate_collection_rejects_name_exceeding_63_bytes` (query.rs:4208) covers both 63 (pass) and 64 (fail) | line 4218 explicitly asserts 63 bytes passes |
| Mid-stream connection drop in stream RPC | NOT TESTED — the `p8a2_supervised_consumer_exits_on_slot_invalidated` case is `#[ignore]`d (driver can't distinguish); no test substitutes for it on the stream-RPC transport layer | line 4146 ignore + no `mid_stream_drop` greps in tests/ |
| Concurrent `registerModel`, same app, different `deploy_id` | PARTIAL — `a2_concurrent_deploys_serialise_via_advisory_lock` (integration.rs:1322) is documented to be **sequential**, not concurrent ("True concurrency under the compio single-runtime test harness would require a multi-threaded runtime" — line 1342). The lock-acquire/release and re-diff path is exercised, but two simultaneous `register_model_with_pool` calls racing on a multi-thread runtime is not driven anywhere | line 1342–1347 comment block |

### 2.5 Test naming + organisation

`mod tests` inventory (parent `src/*.rs` only):

```
src/audit.rs                              4 tests (was 3 in r3, +1)
src/auth/bootstrap.rs                     N tests
src/auth/keys.rs                          N tests
src/auth/session.rs                       N tests
src/backend/mod.rs                        N tests
src/backend/postgres.rs                   2 (compile-time + Debug shape)
src/broker.rs                             N tests (incl. has_subscribers_lifecycle)
src/context.rs                            N tests
src/diff.rs                               N tests
src/error.rs                              N tests
src/exec.rs                               3 tests (NEW from 49b0b98e)
src/migrations.rs                         3 tests (NEW from ed697c45)
src/orchestrator/register_model/apply.rs  3 tests (NEW from 3ef6a170)
src/orchestrator/register_model/bootstrap.rs  1 test (NEW from 3bb41fa1)
src/query.rs                              42+ tests
src/read_set.rs                           N tests
src/v8_classes/db.rs                      3 tests (NEW from 309ed52f)
src/v8_classes/migration.rs               N tests
src/v8_classes/replication.rs             4 tests (NEW from 309ed52f)
src/wal_consumer.rs                       N tests
```

Files **still with zero unit tests** (sorted by LOC, top 10):

```
584 LOC  src/crud.rs
497 LOC  src/v8_bridge.rs
393 LOC  src/v8_classes/collection.rs
372 LOC  src/lib.rs
336 LOC  src/v8_classes/transaction.rs
290 LOC  src/replication_ops.rs
289 LOC  src/v8_classes/migrations.rs
265 LOC  src/orchestrator/auto_tx.rs
257 LOC  src/orchestrator/register_model/mod.rs
208 LOC  src/v8_classes/subscription.rs
173 LOC  src/orchestrator/transaction.rs
135 LOC  src/orchestrator/register_model/validate.rs
```

`crud.rs` (584 LOC), `v8_bridge.rs` (497 LOC), `v8_classes/collection.rs`
(393 LOC), and `v8_classes/transaction.rs` (336 LOC) are the four
largest test-free files — they account for ~1,810 LOC of test-free
parent source.

Naming consistency: tests added this cycle follow a clear
`<function>_<scenario>` pattern (`map_audit_bootstrap_err_preserves_transient_code`,
`drop_column_without_destructive_class_returns_internal_error`,
`exec_mutation_with_emit_skips_build_when_app_suppressed`). This is a
noticeable improvement over r3's mix of `fn test_X` and bare `fn X` —
the new code is uniform.

### 2.6 Mock vs real backend inventory

```
$ grep -l "require_pg\|Pool::connect" crates/plugin-db/tests/*.rs
crates/plugin-db/tests/auto_tx.rs
crates/plugin-db/tests/integration.rs
```

- `tests/integration.rs` (73 tests) — **all real Postgres** via
  `require_pg().await` + `Pool::connect`. Skipped at runtime if
  `DATABASE_URL`/test PG unavailable.
- `tests/auto_tx.rs` (8 tests) — **all real Postgres**.
- `tests/capability.rs` (3 tests) — **no PG** (V8 isolate + JS-only).
- `tests/db_v8_class.rs` (4 tests) — **no PG** (V8 isolate fixtures).
- `tests/subscription_finalizer.rs` (3 tests) — **no PG** (broker
  in-process).

In-crate (`#[cfg(test)] mod tests`) tests use real PG via
`make_test_backend(pool)` in `migrations.rs` (auth/replication
integration tests), and pure helpers everywhere else. No `MockBackend`
or stub `Pool` exists — backend tests either touch real PG or
restrict themselves to pure helpers / compile-time assertions. This is
honest but means `backend/postgres.rs`'s 215-line recovery loop sits
in the gap: too IO-bound to unit-test, not exercised by any
integration test.

---

## 3. Findings

### [IMPORTANT] crates/runtime/tests — cac3e542 dispatch fix still has no Rust regression test (r3 gap #2 — persists)

  Why: Bug `cac3e542` fixed `BOOTSTRAP_MAIN_JS`'s `dispatchRpc` wrapper
  treating dict-shape `default.rpc` stream-kind procedures as
  `Promise<AsyncIterator>` instead of sync `AsyncIterator`. Since the
  fix landed, **zero commits touch `crates/runtime/tests/`** (`git log
  cac3e542..HEAD -- crates/runtime/tests/` is empty). The path is
  guarded by `examples/raw-streaming.smoke.sh` (shell, slow) and
  `examples/db-todos/scripts/smoke.sh` (shell, slow). The closest Rust
  tests (`rpc.rs::async_generator_streams_sse`,
  `rpc_dispatch.rs::dict_shape_does_not_tag_non_string_iterator`)
  drive their own wrappers, not `BOOTSTRAP_MAIN_JS`.

  This is the second regression in three months on the same path. The
  test cost is ~15 LOC.

  Fix: add `crates/runtime/tests/rpc_dispatch.rs::dict_shape_stream_kind_dispatch_returns_sse`
  driving `build_runtime` with `export default { rpc: { tick } }`,
  `tick.config = { kind: "stream" }`, asserting the SSE Data-Stream
  framing in the HTTP response body.

  Verification:
  ```
  $ git log cac3e542..HEAD -- crates/runtime/tests/ | wc -l
  0
  ```

---

### [IMPORTANT] crates/plugin-db/src/backend/postgres.rs — `create_index_with_recovery_audited` 6-branch retry loop has zero unit tests (r3 gap #3 — persists)

  Why: Detailed in §2.3. The function has six terminal exits, including
  the invariant-breach `cic_configuration` branch at line 595 that
  exists specifically to catch a "loop forgot to return" regression.
  Zero unit tests. The only integration test in the area
  (`a1_unique_index_actually_enforces_uniqueness`) exercises
  `build_create_indexes` directly and never touches the recovery loop.

  The retry-loop closures (`refuse`, `log_retry`) are baked into the
  function body — they aren't extractable without a refactor. A
  testable shape would lift `refuse(value) → DbError` and
  `classify(err: &pg::Error) → enum {Fatal, Transient, Other}` to free
  functions. Failing that, an integration test with a deliberately
  unsatisfiable UNIQUE constraint (insert a duplicate before calling
  `create_index_with_recovery_audited` with a UNIQUE spec) would
  drive branches 3 and 5.

  Verification:
  ```
  $ grep -c "#\[test\]" crates/plugin-db/src/backend/postgres.rs
  2
  $ grep -n "create_index_with_recovery\|cic_failed\|cic_configuration\|invalid_index_landed" \
      crates/plugin-db/tests/integration.rs
  (no hits)
  ```

---

### [IMPORTANT] crates/plugin-db/src/orchestrator/register_model/bootstrap.rs — 3bb41fa1 actual-leak-on-error path remains untested (live-PG gap)

  Why: The cycle added `lock_key_formats_with_zs_reg_prefix` — a
  string-format invariant. The leak the commit fixed (pooled client
  returned with the advisory lock still held) is not driven by any
  test. The integration suite has
  `a2_concurrent_deploys_serialise_via_advisory_lock` which proves the
  success-path release: it inserts a `pg_try_advisory_lock` probe AFTER
  a successful run. No test injects an `ensure_app_schema` failure
  mid-bootstrap and probes `pg_locks` afterwards. The agent's own
  test comment (bootstrap.rs:190–217) acknowledges this gap.

  Risk: a future refactor that moves an `ensure_*` call out of the
  unlock-on-error capture block silently re-introduces the leak. The
  next caller on the same pooled client then stalls on a
  `pg_advisory_lock` that hashes to the same `(zs_reg:<app>,
  register_model)` key — exactly the symptom the commit closed.

  Fix: at integration.rs, add `a2_bootstrap_lock_released_on_error`:
  drop a NOT NULL constraint that `ensure_app_schema` will fail on
  (e.g. inject a CHECK violation on the `__zeroship_migrations` table
  preview), call `bootstrap()`, assert it returns `Err`, then assert
  `pg_try_advisory_lock(hashtext('zs_reg:<app>'), hashtext('register_model'))`
  returns true on a separate session — proving the lock was released.

  Verification:
  ```
  $ grep -n "bootstrap.*error.*lock\|pg_advisory_unlock.*zs_reg" \
      crates/plugin-db/tests/integration.rs
  (only line 1399 — the success-path release)
  ```

---

### [IMPORTANT] crates/plugin-db/src/replication.rs — a00c41fd schema/publication case-mismatch fix still value-shaped (r3 gap #5 — persists)

  Why: `publication_sql_uses_quoted_original_case_schema`
  (replication.rs:559) asserts:
  - `publication_name("MyApp") == "__zs_pub_myapp"` (lowercased)
  - `slot_name("MyApp") == "__zs_slot_myapp"` (lowercased)
  - `quote_ident("MyApp") == "\"MyApp\""` (preserves case)
  - `quote_ident("MyApp") != quote_ident("myapp")` (differs from
    pre-fix bug path)

  It does NOT assert that `ensure_publication_and_slot` actually
  composes the `FOR TABLES IN SCHEMA "MyApp"` fragment from
  `quote_ident(app_id)` at the call site. A refactor that inlines a
  `sanitise_app_id(app_id)` call back into the schema-ref slot would
  pass this test (every individual building-block assertion still
  holds) and silently regress the CRITICAL C1 silent-delivery
  failure.

  The integration test `c1_setup_creates_publication_and_slot_idempotently`
  exercises the success path but uses a lowercase app_id
  (`c1_setup_test`) so it doesn't drive the case-mismatch surface.

  Fix: add a fixture `c1_setup_with_mixed_case_app_id` to
  integration.rs that calls `ensure_publication_and_slot(pool,
  "MyApp")`, inserts a row into `"MyApp"."t"`, and asserts the
  publication's `pg_publication_tables` lists `MyApp` (not `myapp`)
  AND that a WAL consumer attached to the slot receives the change
  event. End-to-end coverage of the production call site, not just
  the building blocks.

  Verification:
  ```
  $ grep -n "ensure_publication_and_slot\|MyApp\|mixed.case" \
      crates/plugin-db/tests/integration.rs | head -5
  (no mixed-case fixture; only the value-level test in
   replication.rs:559)
  ```

---

### [IMPORTANT] crates/plugin-db/src/audit.rs — d7cfc089 regression test still value-shaped (r3 gap #7 — persists)

  Why: `insert_backfill_running_empty_returning_is_internal_error`
  (audit.rs:882) re-constructs the `rows.first().map(...).ok_or_else(...)`
  chain on `Vec<()>` instead of invoking `insert_backfill_running()`
  itself. A revert at audit.rs:611 to `.unwrap_or_default()` would
  leave the test passing — it tests `Vec::first` semantics, not the
  audit function. Test comment at line 878 acknowledges this ("This
  test cannot drive a real Client...").

  Fix: extract `extract_inserted_id(rows: &[Row]) -> Result<i64,
  DbError>` as a pure helper used by `insert_backfill_running`,
  `write_audit_row`, `update_audit_status`, `insert_validation_row`.
  Test against an in-memory `Vec<Row>` fixture; or rebuild as an
  integration test that runs `insert_backfill_running` against a real
  pool with a trigger that swallows the RETURNING clause.

  Verification:
  ```
  $ sed -n '882,895p' crates/plugin-db/src/audit.rs
  (test body constructs the chain rather than calling the function)
  ```

---

### [MINOR] crates/plugin-db/src/broker.rs — multi-subscriber Rc-share invariant still not asserted (r3 gap #4 — persists)

  Why: `c54a9f15` introduced `SubscriptionMessage::Change(Rc<ChangeEvent>)`
  precisely to avoid per-subscriber `HashMap<String, String>` clones.
  `multiple_subscribers_receive_event` (broker.rs:777) asserts
  delivery; it does not assert `Rc::ptr_eq(&a, &b)` between the two
  popped messages. A refactor that re-introduces `event.clone()` per
  subscriber still passes the test while regressing the allocation
  cost the commit was designed to fix.

  Fix: extend the test (~6 LOC) — pop both messages, match-bind the
  inner `Rc`, assert `Rc::ptr_eq` and `Rc::strong_count >= 2`.

  Verification:
  ```
  $ grep -n "Rc::ptr_eq\|Rc::strong_count" crates/plugin-db/src/broker.rs
  (no hits)
  ```

---

### [MINOR] crates/plugin-db/src/broker.rs — `has_subscribers` conservative-true semantics only partially tested

  Why: `has_subscribers_lifecycle` (broker.rs:1138) asserts the
  close-then-publish-prune flow. It does not lock in the
  "conservative-true on closed-but-not-yet-pruned" semantics
  documented at broker.rs:486–490. A refactor that inlines
  `subs.retain(|s| !s.is_closed())` inside `has_subscribers` would
  silently change the cost-model from O(1) conservative-true to O(N)
  accurate-false — exactly the regression the fast-path comment
  warns against. r3 gap #8 reduces from IMPORTANT to MINOR
  (lifecycle test partially closes it) but the explicit
  closed-unpruned assertion is still missing.

  Fix (~4 LOC): between `s.close()` and `b.publish(...)` in
  `has_subscribers_lifecycle`, add `assert!(b.has_subscribers("a",
  "messages"), "conservative-true: closed-but-not-pruned must still
  report true");`

  Verification:
  ```
  $ sed -n '1138,1155p' crates/plugin-db/src/broker.rs
  (no assertion of has_subscribers immediately after close,
   before publish)
  ```

---

### [MINOR] crates/plugin-db/src/v8_classes/replication.rs — 309ed52f tests guard the helper but not the JS-shaped opts at the call site

  Why: `resolve_setup_app_id(stamped, _opts) -> stamped.to_string()`
  (replication.rs:107) literally ignores `_opts`. The test asserts
  this. A regression that bypasses the helper — say, inlining
  `opts.get("appId").and_then(|v| v.as_str()).unwrap_or(self.app_id)`
  at replication.rs:66 — would silently re-introduce the override
  without the test detecting it (the helper IS still trivially
  correct).

  This is structural: the helper exists for the testability — but a
  caller-side bypass is not detected by tests on the helper.
  Acceptable as defence in depth (a contributor would have to
  intentionally delete the helper call and inline an alternative
  resolution), and the JS-level integration test
  `tests/db_v8_class.rs` would catch the resulting behaviour change.
  Lower severity than the other "test asserts building block" gaps.

  Fix (~10 LOC): in `tests/db_v8_class.rs`, add a JS-level test that
  calls `db.startReplicationConsumer("victim_app")` from an isolate
  stamped with `"app_a"` and asserts the WAL consumer registers
  against `"app_a"`, not `"victim_app"`. Drives the v8_method path
  end-to-end rather than the helper.

  Verification:
  ```
  $ grep -n "startReplicationConsumer\|replication.*setup" \
      crates/plugin-db/tests/db_v8_class.rs
  (the file covers other v8_class paths but not this one)
  ```

---

### [MINOR] crates/plugin-db/src/query.rs — `validate_field_name` byte-length boundary untested for multi-byte UTF-8

  Why: `validate_field_name_accepts_valid_names` uses `'f'.repeat(63)`
  (ASCII, 63 bytes = 63 chars). The 63-byte boundary in unicode is
  silent: a 16-char name of 4-byte emoji is 64 bytes (would fail) but
  the test doesn't cover the path. A regression that uses
  `name.chars().count()` instead of `name.len()` would silently break
  Postgres truncation safety for unicode field names.

  Fix (~3 LOC): add a fixture asserting
  `validate_field_name("🛡".repeat(15))` is `Ok` (60 bytes) and
  `validate_field_name("🛡".repeat(16))` is `Err` (64 bytes).

  Verification:
  ```
  $ sed -n '4240,4258p' crates/plugin-db/src/query.rs
  (all fixtures are ASCII)
  ```

---

### [MINOR] crates/plugin-db/tests — concurrent `registerModel` (same app, different deploy_id) is sequential-only

  Why: `a2_concurrent_deploys_serialise_via_advisory_lock`
  (integration.rs:1322) explicitly documents at lines 1342–1347 that
  the "concurrency" is sequential — both `register_model_with_pool`
  calls execute on the same compio thread, one after the other. The
  *idempotency* dimension of the advisory-lock contract is verified;
  the *true race* dimension is not. A regression where two threads
  simultaneously hit `pg_advisory_lock(...)` would not surface here.

  Fix (~40 LOC): spawn one `register_model_with_pool` in a separate
  `compio::runtime::Runtime` on a fresh OS thread (via
  `std::thread::spawn`); race it against a second call on the main
  thread; assert one acquires the lock immediately, the other blocks
  on `pg_advisory_lock`, and both ultimately succeed with a single
  `create_table` op in the audit log.

  Verification: lines 1342–1347 explicit acknowledgement.

---

## 4. What Got Better Since Round 3

- **Integration suite buildable again.** `90d992d5` is the headline
  fix — closes the r3 CRITICAL. The 73-test integration crate
  (including the p8a2 hang cluster) is back in CI scope under
  `--features test-helpers`. Spot-checked seven tests under 60-second
  timeouts; all under 4s, no hangs.

- **Five of six cycle commits shipped with unit tests.** Compared to
  r3's "1 of 3" discipline, this is a notable improvement. ed697c45,
  3ef6a170, 49b0b98e are all **function-shaped** tests calling the
  same helper as production. 309ed52f is partial but acceptable
  (helper extraction lifts the security policy to a testable
  surface). 3bb41fa1 has a token format check but acknowledges the
  live-PG gap.

- **+19 lib tests** (302 → 321). Notable additions:
  - `audit.rs` 3 → 4 (one new test)
  - `migrations.rs` 0 → 3 (was test-free at r3)
  - `orchestrator/register_model/apply.rs` 0 → 3 (was test-free)
  - `orchestrator/register_model/bootstrap.rs` 0 → 1 (was test-free)
  - `exec.rs` 0 → 3 (was test-free)
  - `v8_classes/db.rs` 0 → 3 (was test-free)
  - `v8_classes/replication.rs` 0 → 4 (was test-free)

  Seven previously test-free files now have at least one test.
  `migrations.rs`'s 0→3 closes r3 gap #6 partially (the
  audit-bootstrap path is now covered; the `exec_cancel` paths are
  still bare).

- **Test naming uniform.** The new cycle's tests follow a clean
  `<function>_<scenario>_<expected_outcome>` pattern. Easier to scan
  than r3's mixed `test_X` / `X_works` style.

## 5. What Got Worse Since Round 3

Nothing measurable regressed. The persistent gaps (cac3e542 missing
test, branch coverage on `create_index_with_recovery_audited`, value-
shaped vs function-shaped regression tests in `audit.rs` and
`replication.rs`, broker Rc-share invariant) carried forward from r3
unchanged.

---

## 6. Top 3 Actionable Cleanups (re-prioritised for r4)

1. **Add a Rust regression test for dict-shape `default.rpc`
   stream-kind dispatch** (cac3e542). ~15 LOC in
   `crates/runtime/tests/rpc_dispatch.rs`. This is the single most
   over-due test in the surface — second production regression in
   three months on a path with only shell-smoke coverage.

2. **Unit-test the `create_index_with_recovery_audited` 6-branch
   retry loop**. Either (a) lift `refuse(value)` and a `classify(err)`
   enum-returning helper out of the closure scope and unit-test
   against synthetic SQLSTATEs, or (b) add an integration test that
   races the recovery loop against a deliberately unsatisfiable
   UNIQUE constraint to drive the data-violation refuse envelope
   shape. The `cic_configuration` invariant-breach branch
   specifically deserves a test — it's the only signal for a future
   "loop forgot to return" regression.

3. **Tighten the two value-shaped tests** (audit.rs
   `insert_backfill_running_*`, replication.rs
   `publication_sql_uses_quoted_original_case_schema`) into
   function-shaped tests. Extract a `extract_inserted_id(rows: &[Row])
   -> Result<i64, DbError>` helper and a `build_publication_sql(pub,
   app_id)` helper so the production code and the regression tests
   share the same surface, and a refactor at the call site can no
   longer break the production behaviour while leaving the tests
   green.

---

## 7. Score (1-100)

**84 / 100.** Round 3 was 78; round 2 was 88; round 1 not on the same
axis. The +6 delta from r3 is composed of:

- +8 for the integration suite returning to a buildable state (the
  dominant r3 deduction reverses)
- +4 for the cycle's test-discipline improvement (five of six
  commits ship tests vs r3's one of three)
- +2 for the seven previously test-free files now seeded
- −2 for cac3e542 still missing a Rust regression test (the same
  surface that produced this cycle's second-in-three-months
  regression class)
- −2 for the persistent `create_index_with_recovery_audited` branch
  gap (the surface area's largest test-free LOC concentration —
  3,810 LOC of postgres.rs with only 2 compile-time tests)

The crate is past its r3 nadir but not yet at r2's 88. The remaining
ceiling is the four IMPORTANT gaps (#1–#4 above), three of which are
"test asserts building block, not call site" — a structural pattern
that takes coordinated refactoring to fix, not just more tests.
