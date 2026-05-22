# plugin-db — Test Coverage Review (round 10)

- **Date:** 2026-05-22
- **Scope:** `crates/plugin-db/` (src + tests + benches)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84) · r9 (84)
- **Method:** Read-only. Ran `cargo test -p zeroship-plugin-db --lib`, ran the
  integration suite to confirm shape, ran `cargo bench --no-run` to verify the
  new harness builds. Counted `#[test]` / `#[compio::test]` per file. Inspected
  r8/r9 carry-overs and the three commits landed since r9.

---

## TL;DR

**No movement on the test corpus itself.** Lib count is identical to r9 at
371/371 passing. The integration suite is identical at 73 + 1 #[ignore]. The
post-disk-full baseline matches the pre-cleanup baseline exactly — the disk
incident left no residue.

The three commits since r9 (389749ca, 7bd2187e, 757026e3) collectively added
**zero new `#[test]` annotations**:

- `389749ca` (backend_not_initialized unification) is a 2-edit surface
  refinement; the existing wal_consumer test it could have touched
  (`wal_consumer_new_missing_db_url_returns_configuration`) is for a *different*
  condition (db_url empty → `not_provisioned`, not backend missing →
  `backend_not_initialized`). Correctly unmodified.
- `7bd2187e` (bench harness) is criterion microbenches, not tests. Builds clean
  but does not contribute to coverage.
- `757026e3` (docs hold-out closures) is comments only.

All three r8/r9 carry-overs are **still uncovered**: 4 bare files, 5+ branches
in `create_index_with_recovery_audited`, ASCII-only 63-byte boundary tests.

Net: **+0 vs r9 (84 → 84)**. Plateau is now clearly visible — three rounds
flat. Plateau signal is **strong**.

---

## 1. Lib test count + per-file breakdown

```
cargo test -p zeroship-plugin-db --lib
…
test result: ok. 371 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;
finished in 0.14s
```

Lib tests: **371 passing** (r9: 371, r8: 364, r7: 358). All green, no flakes,
0.14s wall time. Disk-full incident left zero residue — count matches the
pre-cleanup baseline exactly.

Per-file `#[test]` / `#[compio::test]` count:

```
tests   file
 147    src/query.rs
  34    src/context.rs
  29    src/broker.rs
  26    src/wal_consumer.rs
  20    src/read_set.rs
  16    src/auth/session.rs
  14    src/replication.rs
  13    src/error.rs
  11    src/diff.rs
  10    src/v8_classes/replication.rs
   8    src/v8_classes/migration.rs
   6    src/orchestrator/lock_guard.rs
   4    src/replication_ops.rs
   4    src/orchestrator/auto_tx.rs
   4    src/auth/keys.rs
   4    src/auth/bootstrap.rs
   4    src/audit.rs
   3    src/v8_classes/db.rs
   3    src/orchestrator/register_model/apply.rs
   3    src/migrations.rs
   3    src/exec.rs
   2    src/v8_classes/subscription.rs
   2    src/backend/postgres.rs
   1    src/backend/mod.rs
   0    src/v8_classes/transaction.rs        ← bare
   0    src/v8_classes/mod.rs
   0    src/v8_classes/migrations.rs
   0    src/v8_classes/collection.rs         ← bare
   0    src/v8_bridge.rs                     ← bare
   0    src/orchestrator/transaction.rs
   0    src/orchestrator/register_model/{validate,plan,mod,bootstrap}.rs
   0    src/orchestrator/mod.rs
   0    src/lib.rs
   0    src/crud.rs                          ← bare
   0    src/auth/mod.rs
```

Identical distribution to r9. The four originally-flagged bare files
(`crud.rs`, `v8_bridge.rs`, `v8_classes/collection.rs`,
`v8_classes/transaction.rs`) remain at zero — third consecutive review with no
movement.

---

## 2. Integration suite stability

```
cargo test -p zeroship-plugin-db --test integration --features test-helpers
…
test result: FAILED. 55 passed; 17 failed; 1 ignored; finished in 11.10s
```

- **73 total test functions** in `tests/integration.rs` (line-count of
  `#[test]`/`#[compio::test]` annotations) + **1 #[ignore]**
  (`p8a2_supervised_consumer_exits_on_slot_invalidated` at line 4175).
- Shape **identical to r9** (73 + 1).
- Failures are environmental — they require a live Postgres at `$DB_URL` and
  fail with connection errors. The failing names (`b8c_per_app_role_…`,
  `c1_watchdog_…`, `p8a2_consumer_publishes_wal_event_to_broker`,
  `aggregate_having_postgres_docs_example`, etc.) all point at DB-touching
  paths.
- Sanity re-run confirmed non-determinism in *which* tests fail (one run: 17
  failed; rerun: 15 failed) — typical of order-sensitive shared-DB tests, but
  the failure set is consistently the Postgres-dependent subset.
- These are not regressions and have been the steady-state for many rounds.
- Note: this *is* a latent concern — flaky environmental-failure tests in a
  test suite that the CI gates on. Not a coverage gap, but a reliability
  signal. (Flagged in r6/r7; still unaddressed.)

---

## 3. Bench harness (`bench_query_build.rs`)

```
cargo bench -p zeroship-plugin-db --no-run
…
Finished `bench` profile [optimized] target(s) in 7.47s
  Executable benches/bench_query_build.rs (target/release/deps/…)
```

- **Builds clean.** 61 warnings (lib, pre-existing) + 23 lib-test warnings;
  zero from the bench file itself.
- Three groups: `build_find/{empty,small,complex}` and
  `build_insert/small_doc`. Each exercises the `validate_collection` byte-prefix
  check transitively (the perf r1 closure path), plus realistic filter/insert
  paths.
- **Does criterion's bench harness count toward test coverage?** No. Criterion
  measures throughput, not correctness — `black_box` discards results without
  assertion. The `.expect(...)` calls on lines 131 and 156 are panic-on-error
  guards, not behaviour checks. The harness defends against *performance*
  regressions, not *behavioural* regressions.
- Indirect coverage benefit: if `build_find` or `build_insert` panic on the
  fixtures, `cargo bench` will fail loudly. That catches the "bench-fixture
  drift after API rename" failure mode for free. Tiny, but non-zero, signal.
- Net effect on coverage score: **0**. The harness is a perf-review enabler,
  not a coverage instrument. Listed here only because r9 explicitly asked to
  verify it.

---

## 4. R8/R9 carry-overs — status check

### 4a. `create_index_with_recovery_audited` — 5+ branches

`crates/plugin-db/src/backend/postgres.rs:385-…` — confirmed still untested
(file has 2 `#[test]` annotations, both for unrelated helpers).

Uncovered branches verified by reading lines 385-499:
1. INVALID-index-lands → drop → retry loop (line 458-487).
2. MAX_RETRIES exhausted → `refuse(validation_refused)` envelope (line 488-499).
3. The `unwrap_or_else` fallback on the envelope serialisation (line 415-417)
   — defensive-only path, unreachable in practice but defensively wired.
4. `log_retry` audit-row construction with `unique` toggling the `change_class`
   between `Compatible` and `Additive` (lines 431-435).
5. The `pool.query_text_params(&check_sql, …).await?` Postgres-error short
   circuit on the indisvalid lookup (line 466).

Five-plus branches, zero direct unit coverage. Integration tests touch the
happy path implicitly via `register_model` flow, but the failure-path branches
(audit writes, drop-and-retry, MAX_RETRIES envelope) remain unexercised. **No
movement since r8.**

### 4b. Four bare files

| file | LOC | tests | status |
| --- | --- | --- | --- |
| `src/crud.rs` | — | 0 | still bare |
| `src/v8_bridge.rs` | — | 0 | still bare |
| `src/v8_classes/collection.rs` | — | 0 | still bare |
| `src/v8_classes/transaction.rs` | — | 0 | still bare |

Three consecutive reviews with zero change. These are infra modules — v8_bridge
is glue, the v8_classes are #[v8_class]-attribute-driven wrappers, crud.rs is
the public CRUD entry façade. The argument that they are "tested indirectly via
db_v8_class.rs / integration.rs" remains the same defence given at r6-r9.
**No movement.**

### 4c. Unicode 63-byte boundary

`crates/plugin-db/src/query.rs:4208`:
```rust
fn validate_collection_rejects_name_exceeding_63_bytes() {
    let name = "a".repeat(64);
    …
    assert!(validate_collection(&"a".repeat(63)).is_ok(), "63-byte name should pass");
}
```

Still ASCII-only. No multibyte tests covering:
- A 21-character string of 3-byte chars (UTF-8 budget = 63 → must pass).
- A 22-character string of 3-byte chars (UTF-8 budget = 66 → must fail).
- A 16-character string of 4-byte chars (UTF-8 budget = 64 → must fail).
- A mixed-width string straddling the boundary.

Postgres `NAMEDATALEN` is *bytes*, not characters, and the implementation
correctly uses `name.len()` which returns byte length — but the *test* asserts
this is bytes only on ASCII inputs, which is the trivial case (`.chars().count()
== .len()`). The interesting case (multibyte) is unverified. **No movement
since r8.** Same wording as r9 — verified unchanged.

---

## 5. Coverage of recent commits

### 389749ca — `backend_not_initialized` code unification

Two-line edits at `v8_classes/migration.rs:269` and `v8_classes/migrations.rs:180`.
Confirmed:

```
src/v8_classes/migration.rs:269:    crate::error::DbError::config(
                                       "backend_not_initialized",
                                       "db: backend not initialized")
src/v8_classes/migrations.rs:180:    "backend_not_initialized",
src/orchestrator/register_model/mod.rs:127:    code: "backend_not_initialized",
```

The audit prompt asks whether `wal_consumer_new_missing_db_url_returns_configuration`
"still asserts the right code". **It asserts `not_provisioned`, not
`backend_not_initialized` — and that is correct.** These are two distinct
conditions:

- **`backend_not_initialized`**: `context.backend()` returns `None` after a
  successful WalConsumer construction (i.e., backend wasn't wired). Surfaced
  from v8_classes paths.
- **`not_provisioned`**: `WalConsumer::new` called with empty `db_url`
  (operator-forgot-to-configure path). Surfaced from `wal_consumer.rs::new`.

The unification commit only touched the *first* condition. The test pins the
*second* condition. They live on parallel rails, both correctly asserted in
their respective tests. **No test impact; correctly unmodified.**

What's *missing*: no direct unit test asserts the new
`backend_not_initialized` code at either of the two unified sites. The change
is observed transitively (e.g., through integration tests that touch
`migrations`), but a string-match unit test on the SDK-visible code would
buy a cheap regression net. Pre-existing gap, not introduced by 389749ca.

### 7bd2187e — bench harness

Covered in §3 above. Zero test contribution; clean compile.

### 757026e3 — docs hold-out closures

Comment-only edits in audit.rs, error.rs, orchestrator/mod.rs, query.rs. Zero
test impact. Correctly so.

---

## 6. Disk-full incident — clean recovery

The disk-full event between r9 and r10 cleared the target/ directory.
Confirmed by re-running on a fresh build:

- **Lib count post-cleanup: 371** — identical to r9's pre-cleanup baseline.
- **Integration shape post-cleanup: 73 + 1 #[ignore]** — identical to r9.
- **Bench harness builds clean from a cold target/.**

No coverage was lost in the incident. The test corpus is git-tracked; only the
build cache was cleared. Re-verified that all 371 lib tests still pass green.

---

## Score (1-100)

```
Round  Score  Delta  Notes
-----  -----  -----  ------------------------------------------------------
r7     83     —      Baseline after a sustained bump cycle.
r8     84     +1     Wave of new tests; carry-overs surfaced.
r9     84      0     One behavioural test; rest were docs/surface.
r10    84      0     Zero new tests. Three commits: surface + bench + docs.
```

**Score: 84** — unchanged from r9 (and r8).

### Why not higher

- All three r8/r9 carry-overs remain open (bare files, audited-CIC,
  multibyte boundary). Three rounds with zero progress on a static list.
- 389749ca *could* have come with a unit test for the unified code at the
  new sites; the change shipped without one. Small but illustrative — the
  default-no-test-with-surface-change pattern is now visible.
- Integration suite has stable but real environmental flakiness (15-17 of 73
  fail under naïve invocation) — a coverage-adjacent reliability issue that
  has been in the long tail since r6.

### Why not lower

- 371 lib tests, 100% green, 0.14s — a healthy, fast, focused unit corpus.
- Per-file distribution is reasonable for module weight (query.rs at 147
  tests, context.rs at 34, broker.rs at 29 — all proportionate).
- The new bench harness is well-scoped, well-documented, and builds clean. It
  is the right scaffold for future perf-cycle measurement.
- 757026e3's restraint (doc-only, no test churn) is correct discipline — the
  pattern of *not* changing tests when behaviour is unchanged is healthy.
- Recent commits do not regress anything. Code-unification work is the kind
  of slow, careful API-surface tightening that *should* not require new tests
  when paired with the existing behavioural coverage.

### Plateau signal: STRONG

Three consecutive rounds at 84 with the same carry-over list and same
defensive responses ("tested indirectly", "low-priority infra", "ASCII covers
the byte path") describe a **stable terminal state** for the current
testing strategy on this crate. Further coverage-lens reviews on this crate
are very unlikely to move the score without:

1. A decision to invest in unit tests for `create_index_with_recovery_audited`
   (3-5 tests against an embedded mock pool, or accept the integration-only
   coverage as policy).
2. A decision on the four bare files — either add façade-level unit tests, or
   formally classify them as "tested via downstream" and stop flagging.
3. A decision on multibyte boundary — a single 3-test addition closes it.

Without one of those decisions, r11/r12 will land at 84 again. Recommend
**switching the lens** (e.g., back to API-surface, performance, security, or
docs-audit) for the next cycle, or **escalating** one of the three open
decisions to the user.

---

## Tool results

```
cargo test -p zeroship-plugin-db --lib                 → 371 passed (0.14s)
cargo test -p zeroship-plugin-db --test integration
   --features test-helpers                             → 55-57 passed,
                                                         15-17 env-failed,
                                                         1 #[ignore]
                                                         (DB-dependent flakes,
                                                         not regressions)
cargo bench -p zeroship-plugin-db --no-run             → bench harness builds
                                                         clean (7.47s)
```

## Files reviewed

- `/home/ruiyang/Projects/appbase/crates/plugin-db/Cargo.toml`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/benches/bench_query_build.rs`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/wal_consumer.rs` (test region 970-996)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/migration.rs` (line 269)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/v8_classes/migrations.rs` (line 180)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/mod.rs` (line 127)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/postgres.rs` (lines 348-499)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/query.rs` (test region 4155-4250)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/tests/integration.rs` (line 4175 #[ignore])
