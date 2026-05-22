# plugin-db Test-Coverage Review — Round 3 (2026-05-22)

Commits evaluated since round 2 (top of branch is `d2e7e22`):

- `5be3c1a1` re-promoted `broker` + `v8_classes` to `pub` (had over-demoted in `2fe9e9f0`)
- `cac3e542` fixed `dispatchRpc` async-wrapper for stream-kind procedures
  (`crates/runtime/src/core/init.rs` + `sdks/bootstrap/src/dev-entry.ts`)
- `a00c41fd` fixed replication schema/publication case mismatch
  (`crates/plugin-db/src/replication.rs`)
- `d7cfc089` fixed audit silent `id=0` from empty RETURNING
- `c54a9f15` shared `ChangeEvent` via `Rc` in broker fanout
- `78a95d3b` added `has_subscribers` fast-path predicate
- `967a7362` early-return in `emit_for_tuple` with no subscribers

---

## 1. Headline

**Overall score: 78 / 100** (down from r2's 88).

Lib test count is up (`298 → 302`) and the three recent point-fix
regressions (`a00c41fd`, `d7cfc089`, plus the dict-shape stream
dispatcher) have varying test coverage — strong for `a00c41fd` and
`d7cfc089`, **absent** for the dict-shape stream dispatcher. The hard
regression in this cycle is that **the external integration test crate
no longer compiles under `--features test-helpers`**: `5be3c1a1` fixed
two of the ten module visibilities the integration suite needs, but
left eight others (`audit`, `auth`, `exec`, `migrations`,
`orchestrator`, `replication`, `replication_ops`, `wal_consumer`) at
`pub(crate)`. So the entire 4,400-line integration suite — including
the p8a2 ordering-hang harness — is currently dead code as far as CI
is concerned. The lib-only test target masks this; round 2's claim of
"72 `#[compio::test]` integration functions covered" is no longer
true at HEAD.

Evidence:

```
$ cargo test -p zeroship-plugin-db --lib 2>&1 | tail -3
test result: ok. 302 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out

$ grep -rn "fn test_\|#\[test\]\|#\[tokio::test\]\|#\[compio::test\]" \
    crates/plugin-db/src/ crates/plugin-db/tests/ | wc -l
512

$ cargo build -p zeroship-plugin-db --tests --features test-helpers 2>&1 | tail -1
error: could not compile `zeroship-plugin-db` (test "integration") due to 118 previous errors

$ cargo build -p zeroship-plugin-db --tests --features test-helpers 2>&1 \
    | grep "^error\[E0603\]" | sort -u
error[E0603]: module `audit` is private
error[E0603]: module `auth` is private
error[E0603]: module `exec` is private
error[E0603]: module `migrations` is private
error[E0603]: module `orchestrator` is private
error[E0603]: module `replication` is private
error[E0603]: module `replication_ops` is private
error[E0603]: module `wal_consumer` is private
```

(The `E0282 cannot infer type` cascade — 110 of the 118 errors — is a
downstream effect of `mig::` going unresolved; once the visibility is
fixed the type inference recovers.)

---

## 2. Findings

### [CRITICAL] crates/plugin-db/tests — `cargo build --tests --features test-helpers` is broken; the integration suite cannot run

  Why: `5be3c1a1` re-promoted only `broker` + `v8_classes`. The
  integration crate also references `audit`, `auth`, `exec`,
  `migrations` (aliased as `mig` at line 1469 — drives ~50 call sites),
  `orchestrator::register_model` (16 call sites), `replication`
  (publication_name / slot_name / ensure_publication_and_slot /
  watchdog_query), `replication_ops`, and `wal_consumer`. All eight are
  still `pub(crate)`. The lib test target (`cargo test --lib`) doesn't
  link `tests/integration.rs`, so the 302/302 green light is
  misleading: the entire p8a2 hang regression harness (commit
  `2b4aff4b`'s `c1_cleanup` defensive sweep, all 12 `p8a2_*` test fns
  at lines 3211, 4026, 4148, 4238, 4289, 4332), the four-phase
  register_model coverage, the migration lifecycle tests, and the
  audit-row tests are currently unbuildable. CI would have caught this
  the moment a non-`--lib` job ran.

  Fix: add an `audit`, `auth`, `exec`, `migrations`, `orchestrator`,
  `replication`, `replication_ops`, `wal_consumer` `pub` block in
  `crates/plugin-db/src/lib.rs` mirroring the existing `broker` / `query`
  / `v8_classes` exceptions, AND add a CI step that runs `cargo build
  -p zeroship-plugin-db --tests --features test-helpers` so the next
  api-surface demotion is caught at build time rather than next time
  someone runs the integration suite.

  Verification:
  ```
  $ cargo build -p zeroship-plugin-db --tests --features test-helpers \
      2>&1 | grep "^error" | head -1
  error[E0603]: module `audit` is private
  ```

---

### [CRITICAL] crates/runtime/tests — Missing coverage of dict-shape `default.rpc` stream-kind dispatch (cac3e542 regression)

  Why: The bug `cac3e542` fixed was specifically that `BOOTSTRAP_MAIN_JS`
  in `crates/runtime/src/core/init.rs` (lines 736-755) wrapped the
  user's dict-shape `default.rpc` in an `async function dispatchRpc`,
  which made the kernel see `Promise<AsyncIterator>` instead of a sync
  `AsyncIterator` and emit "AsyncIterator from a Promise —
  unsupported". The fix added a sync check for `_kind === "stream" ||
  _kind === "subscription"`. **No automated test drives this exact
  path through the bootstrap.** What exists:

  - `crates/runtime/tests/rpc.rs::async_generator_streams_sse` (line 188)
    uses `wrap_with_synthetic_entry` which carries its own custom
    `_zsRpcAndRespond` shim (see `crates/runtime/tests/common/mod.rs`
    line 53). That shim explicitly `await`s the result then sniffs for
    `Symbol.asyncIterator` — it never goes through
    `BOOTSTRAP_MAIN_JS`'s `dispatchRpc` wrapper at all.
  - `crates/runtime/tests/rpc_dispatch.rs::dict_shape_does_not_tag_non_string_iterator`
    (line 374) puts `streamingHandler.config = { kind: "stream" }` in
    user source, but exposes `default.rpc = { probe }` — `probe` is
    a vanilla async query that internally `await __zsDispatch(dict,
    "streamingHandler", ...)`. So the call site that reaches the
    kernel is `probe` (non-stream), and the wrapped streamingHandler
    is invoked only inside JS via the dispatcher itself. The path
    `cac3e542` fixed — the kernel calling `USER_RPC("streamingHandler",
    ...)` and synchronously receiving an iterator — is never exercised.
  - `crates/runtime/tests/subscription.rs` covers WebSocket subscription
    (a different transport — frames over WS, not SSE / FallThrough).

  This is the second time in three months that a stream-RPC path
  shipped a kernel-visible regression without a Rust test catching it
  (the prior was the `Promise<AsyncIterator>` issue itself). The
  `examples/raw-streaming.smoke.sh` shell script catches it
  end-to-end, but CI gating on a shell smoke is fragile and slow.

  Fix: add `crates/runtime/tests/rpc_dispatch.rs::dict_shape_stream_kind_dispatch_returns_sse`:

  ```rust
  let runtime = build_runtime(r#"
      async function* tick() { yield 1; yield 2; }
      tick.config = { kind: "stream" };
      export default { rpc: { tick } };
  "#);
  let (status, body) = dispatch(&runtime, "tick", r#"{"json":null}"#);
  assert_eq!(status, 200);
  assert!(body.contains("2:[1]\n") && body.contains("d:{}\n"),
          "expected SSE Data-Stream framing, got: {body}");
  ```

  This drives the kernel through `BOOTSTRAP_MAIN_JS`'s `dispatchRpc`
  wrapper at init.rs:736 with a real HTTP request, asserts the sync
  FallThrough path (no Promise wrap) routes to `default.fetch` /
  `sseFromAsyncGen`, and would have caught cac3e542's pre-fix state
  with the literal "AsyncIterator from a Promise — unsupported" body.

  A second `kind: "subscription"` test guarding the same branch is
  cheap and matches the dual-condition at init.rs:751.

  Verification:
  ```
  $ grep -rn "default.rpc.*kind.*stream\|kind: \"stream\".*export default" \
      crates/runtime/tests/ | grep -v "//"
  (no hits — no test exposes a stream-kind handler via default.rpc
   AND drives an HTTP dispatch against it)
  ```

---

### [IMPORTANT] crates/plugin-db/src/backend/postgres.rs — `create_index_with_recovery_audited` has zero unit tests covering its six terminal branches

  Why: `create_index_with_recovery_audited` (lines 387-602) is a
  3-retry recovery loop with six terminal exits:
  1. Success — `indisvalid == true` (line 474)
  2. INVALID index landed, retry budget exhausted (line 491)
  3. SQL fatal violation (UNIQUE / NOT_NULL / FK / CHECK) → refuse
     with `unique_violation` envelope (line 533)
  4. Transient retry (deadlock / disk full / OOM) — re-loops
  5. Non-transient or `MAX_RETRIES` reached → refuse with
     `validation_refused` envelope (line 574)
  6. Loop exits without terminal → `DbError::Configuration {
     code: "cic_configuration" }` (line 595)

  Branch (6) in particular is an invariant-breach branch — if the
  loop body forgets to `return` on a path, that's the only signal.
  Zero `grep` hits for `create_index_with_recovery|cic_failed|
  index_retry|invalid_index_landed|data_violation` in
  `crates/plugin-db/tests/integration.rs` — and the unit-test module
  at `backend/postgres.rs:604` is explicit that this layer only
  exercises compile-time trait-shape assertions.

  Each branch returns a structured JSON envelope (`refuse(...)`); the
  refuse helper itself is also untested for its `serde_json::to_string`
  fallback at line 417 (the `unwrap_or_else` path with the literal
  fallback string `"{\"code\":\"cic_failed\",\"reason\":\"envelope
  serialisation failed\"}"`).

  Fix: extract the envelope-building helpers (`refuse(value)` and
  the `log_retry(...)` row builder) into testable free functions, OR
  add a unit-test module that constructs synthetic
  `compio_postgres::Error` SQLSTATE values (the
  `SqlState::UNIQUE_VIOLATION` etc. constants are usable without a
  live connection) and asserts the envelope shape for each branch.
  Minimum: a 5-test module in `backend/postgres.rs` covering branches
  2, 3, 5, 6, and the `refuse` JSON-fallback path. At
  `crates/plugin-db/src/backend/postgres.rs:644`:

  ```rust
  #[test]
  fn refuse_envelope_shape_unique_violation() {
      let env = refuse_for_test(json!({"code":"unique_violation", ...}));
      assert!(matches!(env, DbError::SchemaRefused { code: "cic_failed", .. }));
  }
  ```

  Verification:
  ```
  $ grep -rn "create_index_with_recovery\|cic_failed\|index_retry\|invalid_index_landed" \
      crates/plugin-db/src/backend/ crates/plugin-db/tests/
  (only source-side references; zero in tests/)
  ```

---

### [IMPORTANT] crates/plugin-db/src/broker.rs — `multiple_subscribers_receive_event` asserts payload delivery but not the Rc-share invariant (c54a9f15)

  Why: `c54a9f15` swapped `SubscriptionMessage::Change(ChangeEvent)`
  to `SubscriptionMessage::Change(Rc<ChangeEvent>)` precisely so a
  collection with N subscribers does N refcount bumps instead of N
  deep `HashMap<String, String>` clones. The behavioural test at
  `broker.rs:777` only asserts that both subscribers receive a
  `Change(_)`. It does NOT pop the messages and prove the inner `Rc`
  is the same pointer (`Rc::ptr_eq`) — so a refactor that reintroduces
  the per-subscriber `event.clone()` would still pass the test
  while regressing the allocation cost the commit was designed to fix.

  Fix: at `broker.rs:776`, extend `multiple_subscribers_receive_event`:

  ```rust
  let m1 = s1.pop().unwrap();
  let m2 = s2.pop().unwrap();
  match (m1, m2) {
      (SubscriptionMessage::Change(a), SubscriptionMessage::Change(b)) => {
          assert!(Rc::ptr_eq(&a, &b), "subscribers must share the same Rc payload");
          assert_eq!(Rc::strong_count(&a), 2, "Rc should be shared, not cloned");
      }
      _ => panic!("expected two Change messages"),
  }
  ```

  This locks in the optimisation as part of the public broker
  contract — without it, the c54a9f15 cost-model claim has no
  regression guard.

  Verification:
  ```
  $ grep -rn "Rc::ptr_eq\|Rc::strong_count" crates/plugin-db/src/broker.rs
  (no hits)
  ```

---

### [IMPORTANT] crates/plugin-db/src/replication.rs — Case-mismatch regression test (a00c41fd) covers the SQL fragment but not the round-trip

  Why: `a00c41fd` added `publication_sql_uses_quoted_original_case_schema`
  at `replication.rs:543` — good defensive test. But it only asserts
  the string `quote_ident("MyApp") == "\"MyApp\""`. It does NOT prove
  that `ensure_publication_and_slot` itself uses `quote_ident(app_id)`
  in the `FOR TABLES IN SCHEMA` clause — a refactor that inlines a
  different identifier source (e.g. reverts to `sanitise_app_id`)
  would still pass this test. The test asserts the building block,
  not the call site. The bug would have been re-introducible without
  re-tripping the assertion.

  The integration test in `tests/integration.rs:2871` does call
  `ensure_publication_and_slot` directly — but as documented above,
  that suite doesn't currently build.

  Fix: at `replication.rs:570`, add a function-level test that
  inspects the generated SQL string the function emits. Either
  refactor `ensure_publication_and_slot` to extract `build_pub_sql`
  as a pure function and assert against it directly, OR add a
  test-only `compio_postgres` stub fixture that captures the SQL
  passed to `pool.query_text_params` and asserts:

  ```rust
  let sqls = capture_published_sql_for_app("MyApp");
  assert!(sqls.iter().any(|s|
      s.contains(r#"FOR TABLES IN SCHEMA "MyApp""#)
      && !s.contains(r#"FOR TABLES IN SCHEMA myapp"#)
  ));
  ```

  Verification:
  ```
  $ grep -n "ensure_publication_and_slot" crates/plugin-db/src/replication.rs
  154:pub async fn ensure_publication_and_slot(
  $ grep -c "ensure_publication_and_slot" crates/plugin-db/src/replication.rs
  3   # one definition + two doc references; no in-module test calls it
  ```

---

### [IMPORTANT] crates/plugin-db/tests — p8a2 ordering-hang stability cannot be verified (integration suite unbuildable)

  Why: The round-3 prompt asked to verify the p8a2 hang fixed in
  `2b4aff4b` still doesn't reproduce. Because the integration crate
  doesn't compile (gap #1 above), the requested command:

  ```
  cargo test -p zeroship-plugin-db --features test-helpers --test integration p8a2 -- --test-threads=1
  ```

  exits with 118 compile errors before any p8a2 test runs. So the
  reliability story for the entire p8a2 cluster — `p8a2_consumer_*`,
  `p8a2_supervised_consumer_*`, `p8a2_auto_spawn_*`, `c1_cleanup`'s
  global slot sweep — is unverified at HEAD.

  Fix: same as gap #1 (re-promote the remaining 8 modules). Once the
  suite builds again, the 5-minute ceiling p8a2-only run can be
  re-introduced as a CI nightly job.

  Verification: see gap #1 evidence block.

---

### [IMPORTANT] crates/plugin-db/src/audit.rs — d7cfc089's regression test is value-shaped, not function-shaped

  Why: `d7cfc089` added test
  `insert_backfill_running_empty_returning_is_internal_error` at
  `audit.rs:881` (the third test in the audit module since round 2).
  The test is honest about its scope (comment line 869: "This test
  cannot drive a real Client, but it directly exercises the
  `ok_or_else` error path at the value level"). It exercises a
  *standalone* `rows.first().map(...).ok_or_else(...)` chain — a
  re-implementation of the production logic — rather than calling
  `insert_backfill_running` itself with a mocked / no-row pool.

  Concretely: if someone reverts the production code at `audit.rs:611`
  back to `.unwrap_or_default()`, the test would still pass — it
  tests `Vec::first` semantics on a `Vec<()>`, not the audit function.
  The regression guard is structurally disconnected from the code it
  guards.

  This is a lower-severity version of gap #4 (test asserts a
  building block, not the call site).

  Fix: refactor `insert_backfill_running` so the
  `rows.first().map(...).ok_or_else(...)` chain is a pure helper
  function taking `&[Row]`, and have BOTH production and test call
  it. OR factor a `RETURNING`-row extractor trait and unit-test
  against an in-memory fixture. The latter scales to the other
  RETURNING-bearing audit writes (`write_audit_row`,
  `update_audit_status`, `insert_validation_row`).

  Verification:
  ```
  $ sed -n '875,900p' crates/plugin-db/src/audit.rs
  # confirms the test body re-constructs the chain rather than
  # invoking insert_backfill_running()
  ```

---

### [MINOR] crates/plugin-db/src/migrations.rs — Still zero unit tests

  Why: Round 2's GAP-4 ("`exec_cancel` not-active path untested") and
  the broader observation that `migrations.rs` (824 LOC) carries zero
  `#[test]` functions are both unchanged at HEAD. No commit since
  round 2 added a unit test to this file. With the integration suite
  unbuildable (gap #1), the entire migration lifecycle is effectively
  untested in CI.

  Fix: extract `err_not_active()`, the audit-generation comparison,
  and the JSON envelope shaping into pure helpers and add a
  `#[cfg(test)] mod tests` block. The advisory-lock pathway needs PG
  but the error-classification + envelope branches don't.

  Verification:
  ```
  $ grep -c "#\[test\]\|#\[compio::test\]" crates/plugin-db/src/migrations.rs
  0
  ```

---

### [MINOR] crates/plugin-db/src/broker.rs — No test for `has_subscribers` (78a95d3b) conservative-true semantics on closed-but-not-pruned subscribers

  Why: `78a95d3b` added `Broker::has_subscribers` as a fast-path
  predicate so `wal_consumer::emit_for_tuple` can skip tuple
  construction when no subscribers are present (`967a7362`). The
  function's doc-comment at `broker.rs:480` documents the
  "conservative-true" semantics: an entry whose subscribers have all
  been closed but not yet pruned still returns true. There is no test
  that locks this in — a refactor that calls `subs.retain(|s|
  !s.is_closed())` inside `has_subscribers` would silently change
  the semantics from `O(1)` conservative-true to `O(N)` accurate-false,
  defeating the fast-path purpose. The behaviour contract is in the
  comment only.

  Fix: at `broker.rs:825`, add
  `has_subscribers_returns_true_for_closed_unpruned`:

  ```rust
  let mut b = Broker::new();
  let s = b.subscribe("a", "m");
  s.close();
  assert!(b.has_subscribers("a", "m"),
      "conservative-true: closed-but-not-pruned must still report true");
  b.publish(&ev("a", "m", ChangeOp::Insert, Some(1)));  // triggers prune
  assert!(!b.has_subscribers("a", "m"),
      "after prune, has_subscribers must return false");
  ```

  Verification:
  ```
  $ grep -n "has_subscribers" crates/plugin-db/src/broker.rs
  (definition + emit_for_tuple's call site; no test references)
  ```

---

## 3. What Got Better Since Round 2

- **Two of three regression fixes have direct tests.** `a00c41fd`
  (replication case mismatch) added
  `publication_sql_uses_quoted_original_case_schema`; `d7cfc089`
  (audit empty RETURNING) added
  `insert_backfill_running_empty_returning_is_internal_error`. Both
  are imperfect (gaps #5 and #7 above) but the discipline of landing
  a test alongside a fix has improved since round 2.

- **Lib test count up to 302.** Net `+4` from r2's 298:
  `replication.rs` 6→7, `audit.rs` 3→4, plus two elsewhere. No new
  tests landed in the previously zero-test files (`exec`, `crud`,
  `migrations`, `v8_bridge`, `replication_ops`, `orchestrator/*`).

- **Broker fanout cost-model commit landed (c54a9f15).** Behavioural
  tests still pass; the optimisation is structurally sound (per-isolate
  single-threaded → `Rc` is correct). The remaining gap (#4) is
  asserting the Rc-share rather than just delivery.

## 4. What Got Worse Since Round 2

- **The integration test crate no longer builds** under the feature
  flag it requires (`--features test-helpers`). Round 2 measured
  ~85% coverage of CRUD / orchestrator / migration / replication via
  integration tests; at HEAD that coverage exists in source only —
  CI sees none of it. This is the dominant reason the overall score
  dropped from 88 → 78.

- **One of three recent regression fixes (cac3e542) has no Rust
  test.** The dict-shape `default.rpc` stream-kind path is now
  guarded only by shell smokes (`examples/raw-streaming.smoke.sh`,
  `examples/db-todos/scripts/smoke.sh`). For a kernel-level invariant
  that's the second regression in this surface, that's thin.

---

## 5. Top 3 Actionable Cleanups (re-prioritised for r3)

1. **Re-promote the eight remaining `pub(crate)` modules needed by
   `tests/integration.rs` (gap #1 / CRITICAL).** This single change
   unblocks the entire integration suite, restores p8a2 stability
   verification, and reactivates the 72-test `#[compio::test]` block
   round 2 took credit for. Pair with a CI step that runs `cargo
   build -p zeroship-plugin-db --tests --features test-helpers` on
   every push so the next api-surface demotion fails fast.

2. **Add a Rust regression test for dict-shape `default.rpc`
   stream-kind dispatch (gap #2 / CRITICAL).** ~15 LOC in
   `crates/runtime/tests/rpc_dispatch.rs`. Drives the actual
   `BOOTSTRAP_MAIN_JS::dispatchRpc` wrapper through `call_fetch_handler`
   with a stream-kind handler exposed via `export default { rpc: {
   tick } }`. Would have caught cac3e542's pre-fix state directly
   instead of waiting for the shell smoke.

3. **Tighten the two value-level regression tests
   (`d7cfc089`, `a00c41fd`) into function-level tests (gaps #5 + #7).**
   Both currently test building blocks rather than the production
   call sites they were written to guard. Extract the
   RETURNING-row-extraction helper and the publication-SQL builder
   so the production code and the regression test share the same
   surface, and the test can no longer pass while the production
   code regresses.

---

## 6. Score (1-100)

**78 / 100.** Round 2: 88. The −10 delta is dominated by the
unbuildable integration crate (a full test suite gone dark in CI).
The two new lib-level regression tests (a00c41fd + d7cfc089) and the
broker Rc-share commit add positive value, but only a fraction of
what the integration suite covered. The dict-shape stream-kind
regression (cac3e542) is a textbook "ship the fix, skip the test"
moment for a path that has now produced two production regressions
in three months.
