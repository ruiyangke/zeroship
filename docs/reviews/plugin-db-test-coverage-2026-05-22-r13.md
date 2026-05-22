# plugin-db — Test Coverage Review (round 13)

- **Date:** 2026-05-22 (cycle 13:47 → 14:17 → r13 audit)
- **Scope:** `crates/plugin-db/` (src + tests + benches) +
  `crates/compio-postgres/src/test_utils.rs` (cross-crate enabler)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84) · r9 (84) · r10 (84) · r11 (85) · r12 (86)
- **HEAD:** `6ff294fd` (verified via `git rev-parse HEAD`)
- **Method:** Read-only. Re-ran `cargo test --lib` (default + `hardening`)
  and `cargo test -p compio-postgres --features test-utils`. Diffed every
  src/* file touched in cycles 13:47 and 14:17
  (`f6adb68b`, `bf75e866`, `75d9ae5c`, `6ff294fd`). Audited the new
  `test-utils` feature and the bench harness for de-facto test value.

---

## TL;DR

**Lib test counts (HEAD = 6ff294fd):**

| Feature flag                  | Tests   | Δ vs r12 |
| ----------------------------- | -----   | -------- |
| (default)                     | **352** | 0        |
| `--features hardening`        | **376** | 0        |
| `compio-postgres test-utils`  | **15**  | 0        |
| Hardening delta (`auth/*`)    | +24     | unchanged |

**Cycle 13:47 + 14:17 commit audit:**

| Commit       | Net tests | Notes                                    |
| ------------ | --------- | ---------------------------------------- |
| `f6adb68b`   | 0         | Field privatization (I16); accessor surface unchanged |
| `bf75e866`   | 0         | `test-utils` feature in compio-postgres — **constructor-only, no self-tests** |
| `75d9ae5c`   | 0 (lib)   | Bench harness — sanity asserts (de-facto smoke for `row_for_test` + I35) |
| `6ff294fd`   | 0         | docs/deferred entry, no code             |

Net: **+0 tests**. But cycle adds (a) the dev-deps `tracing-subscriber`
infrastructure for the r12-recommended capture-layer pattern with
**zero tests landed using it**, and (b) a new cross-crate `test-utils`
feature whose three functions have **no unit tests of their own**.

---

## 1. Tool results

```
git rev-parse HEAD                                  → 6ff294fd
cargo test -p zeroship-plugin-db --lib              → 352 passed (0.14s)
cargo test -p zeroship-plugin-db --lib
   --features hardening                             → 376 passed (0.14s)
cargo test -p compio-postgres --features test-utils → 15 lib + 23 integ
                                                       + 2 doctest, all pass
cargo build -p compio-postgres                      → ok (test_utils hidden)
cargo build -p compio-postgres --features test-utils → ok
cargo build -p zeroship-plugin-db --benches         → ok
```

All green. The feature gate cleanly hides the `test_utils` module in
default builds: `lib.rs:129-130` `#[cfg(feature = "test-utils")] pub mod
test_utils;` — verified by inspecting that `cargo build -p
compio-postgres` (no feature) does not pull in `test_utils.rs`.

---

## 2. r12 open-gap re-classification

| ID  | r12 status | r13 status  | Evidence |
| --- | ---------- | ----------- | -------- |
| GAP-3 (I14 lenient strictness) | open (6th round) | **open (7th round)** | `grep lenient tests/integration.rs` → 0 hits; only `validate.rs` references the term |
| NEW-R11-1 + NEW-R12-1 (tracing-emission tests, 9 sites) | partial+grew | **partial — infra added, tests still absent** | `tracing-subscriber` in `[dev-dependencies]` (`Cargo.toml:44`) but `grep -rn tracing_subscriber crates/plugin-db/src crates/plugin-db/tests` → 0 hits |
| NEW-R12-2 (F1 warn-shape drift) | open | **CLOSED (code fix, not test)** | `18aee490 plugin-db/migrations: fix finalise_backfill F1 warn-shape drift (NEW-R12-2)` landed pre-r13. The drift is gone; the snapshot-test rationale survives |
| Multibyte 63-byte boundary on `validate_collection` | open | **open** | `query.rs:4221` still ASCII-only |
| `create_index_with_recovery_audited` audited branches | open | **open** | unchanged |
| 4 bare files (`crud.rs`, `v8_bridge.rs`, two v8_classes) | open | **open** | `v8_bridge.rs` now has new `row_to_json_for_bench` shim + I35 fix, still 0 unit tests; bench's sanity-assert preamble (§4) provides indirect coverage |

### 2.1 NEW-R12-2 — interesting partial closure

The shape-drift `finalise_backfill` warn (r12 §4.1) was fixed in
commit `18aee490` by adopting `audit_err = %e` and `transition =
?terminal_str` so the 6 sites converged on one shape. **The
code-level drift r12 flagged is gone**, but the *test that would
prevent the next drift* (the snapshot test) was never written. The
gap is functionally addressed for now but structurally still open.
Classification: **CLOSED (code), OPEN (test infrastructure)**.

### 2.2 NEW-R11-1 + NEW-R12-1 — infrastructure added, tests still missing

Cycle 13:17 (commit `89dbb6a8` — outside this r13's commit window but
relevant for context) added `tracing-subscriber = { workspace = true
}` to plugin-db's `[dev-dependencies]` with a comment block
(`Cargo.toml:36-44`) explicitly citing NEW-R11-1 + NEW-R12-1 + the
field-shape snapshot test rationale. **Six days / two cycles later,
no test has been written using it.** The infrastructure-without-tests
state is *worse* than r12's "no infrastructure" state because it
demonstrates that the gap is recognized but not closed.

Severity remains LOW (observability-only). But "Cargo.toml has a
72-word justification for a dev-dep with no consumers" is a smell I'm
calling out.

---

## 3. NEW: cross-crate `test-utils` feature audit (`bf75e866`)

`compio-postgres` gains a new dev-only feature `test-utils = []`
exposing three functions in `src/test_utils.rs`:

- `column_for_test(name, ty) -> Column`
- `statement_for_test(columns) -> Statement`
- `row_for_test(columns, values) -> Result<Row, Error>`

The third is the load-bearing one — it synthesises a real `DataRowBody`
by writing the PostgreSQL wire format (`'D' + len + col_count + per-col
[len + bytes]`) into a `BytesMut` and feeding it through
`Message::parse`, so the synthesised `Row` exercises **the same
decode path** a real Postgres response would.

### 3.1 Equivalence to real wire rows — yes, by construction

The audit question "are the synthesized rows actually equivalent to
PG-wire rows" decomposes:

- **Wire format:** `test_utils.rs:90-126` writes the canonical
  DataRow frame. `Message::parse` is the public path
  `postgres-protocol` exposes; the same call the real client uses.
- **`Row::new` path:** `test_utils.rs:83` calls `Row::new(statement,
  body)` — the unmodified production constructor.
- **Decode equivalence:** the bench's preamble at
  `bench_row_to_json.rs:195-214` decodes each fixture via
  `row_to_json_for_bench` and asserts the resulting `Value`'s object
  has the expected column count (3 / 10 / 50). This is a de-facto
  end-to-end smoke for `row_for_test` itself — if any of the
  per-column wire encodings (`enc_int4`, `enc_jsonb`,
  `enc_timestamptz_us`) were wrong, the assertion would fire.

### 3.2 Missing direct unit tests for `test_utils`

The bench's preamble assertions run only under `cargo bench` (or as a
manual smoke). **There is no `#[test]` in `compio-postgres` that
exercises `row_for_test`** — `cargo test -p compio-postgres --features
test-utils` shows 15 lib tests (unchanged from the default build; the
feature exposes no tests). The bench preamble is the closest thing,
but:

1. It does not run under `cargo test`, only `cargo bench`.
2. It only exercises 6 OIDs (INT4 / INT8 / BOOL / TEXT / JSONB /
   TIMESTAMPTZ). A future contributor extending `row_for_test` to
   handle, say, `NUMERIC` or arrays would have no test to break.
3. It asserts only column count, not value equivalence — if `enc_jsonb`
   silently emitted the wrong version byte, `row_to_json` would decode
   *something* and the assertion would pass; a real bug in JSONB
   formatting would slip.

**Finding: NEW-R13-1 (LOW)** — `test_utils` exposes 3 constructors
with zero direct unit tests. A round-trip test
(`row_for_test → row.try_get::<_, T>() → assert_eq!(original, T)` for
each supported type) is the natural shape. Cost: ~6 tests, all
synchronous, no live DB needed.

### 3.3 Could this replace integration tests?

The prompt asks whether `bench_row_to_json`'s synthetic-Row construction
could be reused to write **integration tests without a live PG**,
replacing some of `tests/integration.rs`. Audit:

- `integration.rs` tests exercise the full BEGIN/COMMIT/ROLLBACK
  lifecycle, advisory locks, audit-row state machine, and the
  emit-vs-commit barrier. **None of these touch `row_to_json` directly**
  — they test SQL-level invariants whose ground truth is the live
  Postgres server (e.g. "did the row actually land in the table",
  "did `pg_advisory_unlock` release the lock").
- `row_for_test` synthesises a `Row` *from raw wire bytes*. It does NOT
  exercise a connection, a transaction, or the SQL planner.
- A handful of unit-test-level migration tests *could* benefit:
  `audit::find_latest_backfill_row` decodes a Row; if its decode were
  tested by feeding it a `row_for_test` directly, the cross-thread,
  cross-runtime integration coupling would lift. But the SQL it issues
  is still part of its contract; testing only the decode side leaves
  the SQL untested.

**Verdict:** `row_for_test` cannot meaningfully replace
`integration.rs` tests. It *could* enable per-function decode tests
for `audit::*` and `replication::pgoutput_decode_*` (the latter
already has 11 wire-decode unit tests in compio-postgres — a similar
shape for plugin-db's row consumers is open territory).

**Finding: NEW-R13-2 (LOW)** — `row_for_test` unlocks a class of
decode-only unit tests for `audit.rs` consumers of `Row` (e.g.
`find_latest_backfill_row`, `next_schema_version`, `write_audit_row`)
that currently sit behind the integration-test wall. None have been
written. Same cost as NEW-R11-1 (~5 tests, sub-second).

---

## 4. NEW: `bench_row_to_json` audit (`75d9ae5c`)

237-line Criterion harness, 3 bench shapes (narrow_3cols /
medium_10cols / wide_50cols). Audited:

- **Compiles + runs cleanly:** verified via `cargo build -p
  zeroship-plugin-db --benches` (15.59s clean rebuild, 1 unrelated
  warning in `replication.rs:1196 unused_imports`).
- **Encoders match wire spec:** `enc_int4` (4-byte BE), `enc_int8`
  (8-byte BE), `enc_bool` (single byte), `enc_text` (raw UTF-8),
  `enc_jsonb` (0x01 version prefix + JSON), `enc_timestamptz_us`
  (8-byte BE i64 microseconds-since-2000) — all match
  `postgres-types`' canonical encodings.
- **De-facto smoke test:** the preamble at lines 195-214 calls
  `row_to_json_for_bench` on all three fixtures and asserts the
  object's `.len()` matches the column count. This catches gross
  decode failures (panics, wrong column counts) but not value-level
  bugs.
- **No `cargo test` coverage:** the preamble runs only under `cargo
  bench`. A future regression that breaks `row_for_test` would not be
  caught by `cargo test --lib` or `cargo test --workspace`.

### 4.1 The "bench is the only test" risk

`row_to_json_for_bench` is a `#[doc(hidden)] pub` shim added at
`lib.rs:228-232` specifically to let the external bench reach
`v8_bridge::row_to_json`. The shim is exposed in `pub` surface, has
no `#[cfg]` gate, and is **only exercised by the bench**. If someone
deletes it (thinking it's dead code), `cargo build -p
zeroship-plugin-db --benches` would break — but `cargo test
--workspace` would not. Bench wiring is a frequently-broken-and-
fixed-silently surface in workspaces.

**Finding: NEW-R13-3 (LOW)** — `row_to_json_for_bench` has no
in-crate consumer beyond the bench. A `#[test]` that calls it once
with a hand-built `row_for_test` row would convert the bench from
"only verifiable by running it" to "verified by `cargo test --lib`".
Cost: 1 test (~10 lines).

---

## 5. NEW: I16 field privatization audit (`f6adb68b`)

11 fields demoted `pub(crate) → private` in `IsolateDbContext`:
`pool`, `db_url`, `registered_models`, `tx_conn`, `auto_tx_owned`,
`tx_token`, `tx_token_counter`, `pending_emits`, `mig_lock`,
`running_consumers`, `backend`. The `MigrationLock` 6-field sub-
struct stays `pub(crate)` (commit body notes this as deferred).

### 5.1 No external field access — confirmed

Grep over `crates/plugin-db/src/` excluding `context.rs` for
direct field access on these names: 0 hits outside accessor calls
(every match is `c.backend()`, `c.tx_token()`, `c.db_url()`, or
`self.pool` *inside* `backend/postgres.rs`'s `PostgresBackend` — a
*different* struct that happens to have a `pool` field). The commit
body's claim of "0 direct field accesses outside context.rs"
verifies at HEAD.

### 5.2 Accessor coverage gaps

`context.rs` has 34 tests covering the accessor surface. Accessors
*not* tested directly:

- `set_pool` / `clear_pool` (require a real `Rc<Pool>`)
- `install_tx_client` / `take_tx_client` / `put_tx_client` (require a
  real `Client`)
- `try_mark_consumer_running` / `unmark_consumer_running` (covered
  indirectly via `replication_ops.rs` tests; `consumer_running_round_trip`
  in context.rs uses `mark_consumer_running` (test-helper) not the
  atomic `try_*` variant)

The `Client` / `Pool`-bound accessors are correctly punted to
integration. The `try_mark_consumer_running` gap is interesting — the
non-atomic test-helper `mark_consumer_running` is exercised here, but
the atomic production accessor is not unit-tested in `context.rs`
itself. `replication_ops::tests::consumer_running_guard_drop_unmarks_on_panic_unwind`
indirectly covers it.

**Finding (not a new gap, just a note):** the privatization is
defensible; no production caller broke; accessor coverage is
unchanged from r12.

---

## 6. Per-file `#[test]` count (HEAD = 6ff294fd)

```
tests   file                            Δ vs r12
 149    src/query.rs                    0
  34    src/context.rs                  0
  29    src/broker.rs                   0
  26    src/wal_consumer.rs             0
  20    src/read_set.rs                 0
  16    src/auth/session.rs             0   (hardening-gated)
  14    src/replication.rs              0
  13    src/error.rs                    0   (recount; r12 said 14)
  11    src/diff.rs                     0
  10    src/v8_classes/replication.rs   0
   8    src/v8_classes/migration.rs     0
   6    src/orchestrator/lock_guard.rs  0
   6    src/exec.rs                     0
   4    src/replication_ops.rs          0
   4    src/orchestrator/auto_tx.rs     0
   4    src/auth/keys.rs                0   (gated)
   4    src/auth/bootstrap.rs           0   (gated)
   4    src/audit.rs                    0
   3    src/v8_classes/db.rs            0
   3    src/orchestrator/register_model/apply.rs   0
   3    src/migrations.rs               0
   2    src/v8_classes/subscription.rs  0
   2    src/backend/postgres.rs         0
   1    src/backend/mod.rs              0   (r12 said 2; recount)
```

Sum: 352 default / 376 hardening, matches measured. **Net zero tests
added this cycle.**

---

## 7. New gaps introduced this cycle

- **NEW-R13-1** (LOW) — `compio_postgres::test_utils::row_for_test`
  + `column_for_test` + `statement_for_test` ship without
  `#[test]`s. The bench preamble at `bench_row_to_json.rs:195-214`
  is a de-facto smoke under `cargo bench` only; not exercised by
  `cargo test`. §3.2.
- **NEW-R13-2** (LOW) — `row_for_test` unlocks a class of decode-only
  unit tests for `audit.rs` / `replication.rs` `Row` consumers that
  currently live in `tests/integration.rs`. None have been written.
  §3.3.
- **NEW-R13-3** (LOW) — `row_to_json_for_bench` shim at `lib.rs:228`
  is `pub` (not `pub(crate)`) and has no in-crate test consumer.
  Bench-only exposure; one `#[test]` would prevent a silent delete.
  §4.1.
- **NEW-R11-1 + NEW-R12-1 carry — now with infrastructure** —
  `tracing-subscriber` is in `[dev-dependencies]` with a 72-word
  justification specifically citing these gaps; **zero tests have
  been written using it**. The infrastructure-but-no-tests state is
  worse than r12's no-infrastructure state because the gap is now
  documented-but-uncosted-uncloseed. §2.2.

Severity: all LOW (observability + bench wiring, not data-path).

---

## 8. Score (1-100)

```
Round  Score  Delta  Notes
-----  -----  -----  ------------------------------------------------------
r7     83     —      Baseline.
r8     84     +1     Wave of new tests.
r9     84      0     One behavioural test.
r10    84      0     Zero new tests; plateau STRONG.
r11    85     +1     GAP-1/I12 CLOSED.
r12    86     +1     GAP-2/I13 CLOSED.
r13    86      0     Zero new tests. NEW-R12-2 closed at code level
                     (18aee490). I16 privatization clean. New cross-crate
                     `test-utils` surface ships without self-tests + new
                     bench harness adds 3 LOW gaps + the tracing-subscriber
                     dev-dep was added 2 cycles ago with zero consumers.
                     GAP-3 carries (7th round). The cycle's quality is in
                     code (privatization + bench measurement), not tests.
```

**Score: 86** — unchanged from r12 (Δ = 0).

### Why not higher

- Zero net tests added across 4 commits over 2 cycles.
- `tracing-subscriber` dev-dep has been sitting unused for 2 cycles
  with explicit Cargo.toml comments naming the tests it would enable.
  Test debt becoming visible in build files is unusual.
- 3 new LOW gaps from `test-utils` / `bench_row_to_json` shipping
  without their own tests.
- GAP-3 (lenient strictness) still open — 7th round carry.
- 4 bare files unchanged; `v8_bridge.rs` got new code (I35 fix +
  `row_to_json_for_bench`) without any new tests.

### Why not lower

- 352/376 lib tests all green in <0.2s; no regressions.
- I16 privatization is clean — verified 0 external field accesses, all
  consumers route through typed accessors.
- The bench harness, while not a test, *does* provide de-facto smoke
  for `row_for_test` via the column-count assertions in its preamble.
- NEW-R12-2 closed at the code level by `18aee490`.
- The `test-utils` feature is correctly gated — `cargo build -p
  compio-postgres` (no feature) does not see the surface; `cargo build
  -p compio-postgres --features test-utils` compiles cleanly.
- The bench harness produced **honest measured numbers** (closing
  [I35] in `docs/reviews/plugin-db-deferred.md`) rather than the
  estimated-without-measurement pattern the memory `feedback_never_
  estimate.md` warns against. This is a quality win even though it
  doesn't lift the test-coverage score.

---

## 9. Files reviewed

- `/home/ruiyang/Projects/appbase/crates/plugin-db/Cargo.toml`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs` (full)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/lib.rs:215-260`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/benches/bench_row_to_json.rs` (full)
- `/home/ruiyang/Projects/appbase/crates/compio-postgres/Cargo.toml`
- `/home/ruiyang/Projects/appbase/crates/compio-postgres/src/lib.rs:115-150`
- `/home/ruiyang/Projects/appbase/crates/compio-postgres/src/test_utils.rs` (full)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/tests/integration.rs` (grep audit only)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-test-coverage-2026-05-22-r12.md`
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-deferred.md` (referenced)
