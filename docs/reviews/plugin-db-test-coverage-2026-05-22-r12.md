# plugin-db — Test Coverage Review (round 12)

- **Date:** 2026-05-22 (cycle 12:47 → r12 audit)
- **Scope:** `crates/plugin-db/` (src + tests + benches)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84) · r9 (84) · r10 (84) · r11 (85)
- **HEAD:** `81226451` (verified via `git rev-parse HEAD`)
- **Method:** Read-only. Re-ran `cargo test --lib` (default + `hardening`).
  Diffed every plugin-db src/* file touched in cycles 12:17 and 12:47
  (`51c342e8`, `fcf7ce3c`, `71a457a1`, `7c6bd2ec`). Audited the new
  caller paths the I6 trait change spawned and the 6-site unified F1
  warn shape for unit-test coverage.

---

## TL;DR

**Lib test counts (HEAD = 81226451):**

| Feature flag                  | Tests   | Δ vs r11 |
| ----------------------------- | -----   | -------- |
| (default)                     | **352** | +3 (was 349) |
| `--features hardening`        | **376** | +3 (was 373) |
| Hardening delta (`auth/*`)    | +24     | unchanged |

The +3 comes entirely from `ae5570dc plugin-db/exec: unit tests for
queue_or_emit / drain / clear (I13)` (`exec.rs` went 3 → 6). Counts
match the cycle 12:47 commit-body claim exactly.

**Cycle-12 commit audit:**

| Commit       | Net tests | Caller-path tests | Warn-event tests |
| ------------ | --------- | ----------------- | ---------------- |
| `ae5570dc`   | **+3**    | n/a               | n/a              |
| `51c342e8`   | 0         | **0** (2 callers) | **0** (2 warns)  |
| `fcf7ce3c`   | 0         | n/a               | **0** (5 warns)  |
| `7c6bd2ec`   | 0         | n/a               | **0** (6 sites)  |
| `71a457a1`   | 0         | n/a (doc-only)    | n/a              |

Net: **+1 vs r11 (85 → 86).** The bump is GAP-2 properly closing
(testable subset shipped), partially offset by **three new commits in
the same cycle that added 7 production warn emissions and 2 new caller
branches with zero test coverage** — feeding NEW-R11-1 (now upgraded
from "one site" to "an established no-test-for-tracing pattern").

---

## 1. Tool results

```
git rev-parse HEAD                            → 81226451
cargo test -p zeroship-plugin-db --lib        → 352 passed (0.14s)
cargo test -p zeroship-plugin-db --lib
   --features hardening                       → 376 passed (0.15s)
```

All green, sub-second.

---

## 2. r11 open-gap re-classification

| ID  | r11 status | r12 status  | Evidence |
| --- | ---------- | ----------- | -------- |
| GAP-1 (I12 non-ASCII) | CLOSED in r11 | **CLOSED** | unchanged since `403b3891` |
| GAP-2 (I13 queue_or_emit) | open | **CLOSED (partial)** | `ae5570dc` adds 3 of 4 branches; in-tx branch still requires a real `Client` (integration-only) |
| GAP-3 (I14 lenient strictness) | open | **open** | `grep lenient tests/integration.rs` → 0 hits, unchanged |
| Multibyte 63-byte boundary on `validate_collection` | open | **open** | `query.rs:4221` still ASCII-only |
| `create_index_with_recovery_audited` audited branches | open | **open + grew** | see §3 |
| Four bare files (`crud.rs`, `v8_bridge.rs`, two v8_classes) | open | **open + grew** | `v8_bridge.rs` now has 2 production warns and still zero tests |
| NEW-R11-1 (tracing-emission tests for `5d9acab8`) | new LOW | **partial + grew** | see §4 |

### 2.1 GAP-2 — CLOSED (partial)

`ae5570dc` lands 3 new tests in `exec.rs` at the `tests` module:

- `queue_or_emit_no_tx_emits_immediately` — verifies the `has_tx == false`
  branch hits `emit_local` (asserted via a real `broker::subscribe`
  consumer popping a `Change` event).
- `drain_pending_emits_on_commit_fires_every_queued_event` — seeds 3
  events via `push_pending_emit_for_tests`, drains, asserts all 3 land,
  then asserts a second drain is a no-op (queue is consumed, not copied).
- `clear_pending_emits_drops_without_firing` — ROLLBACK semantics:
  queued event is dropped silently, post-clear drain is also a no-op.

The fourth branch (in-tx with a real `tx_conn`-parked `Client`) is
covered by `gap_b_subscriber_does_not_observe_pre_commit_state` in
`tests/integration.rs`. The unit-test gap that has been open since r2
is genuinely closed. **First multi-round HIGH-severity GAP closure in
the plugin-db test-coverage track.**

### 2.2 GAP-3 — still open

Unchanged: `grep -rn lenient tests/integration.rs` → 0 hits.
`validate.rs:90-93` "lenient → skip destructive ops" branch still has
no end-to-end test. **6th round carrying.**

---

## 3. NEW: I6 trait-change callers (commit `51c342e8`)

`release_advisory_lock` went from `()` to `Result<(), DbError>`. Two
production callers gained new `if let Err(e) = …` + `tracing::warn!`
branches:

- `migrations.rs:287-297` — cancelled-refusal path.
- `migrations.rs:661-671` — backfill-finalise path.

Neither path has a unit test asserting the `Err` branch fires. The
postgres impl at `backend/postgres.rs:157-173` lifts
`compio_postgres::Error` via `DbError::from_pg`. To test the Err arm
in isolation would require either:

1. A mock `Backend` impl returning `Err`, or
2. A real Postgres connection where `pg_advisory_unlock` fails (hard
   to provoke deterministically).

The `Backend` trait is already mockable shape-wise (associated types,
async fns), but **no test-only mock impl exists in the crate**. The two
existing tests in `backend/postgres.rs` (lines 718, 755) only check
the Debug-impl source and compile-time trait bounds — they don't
exercise the trait at runtime.

**Finding: NEW-R12-1 (LOW)** — the I6 trait-change Err branches have
zero unit-test coverage. A test mock `Backend` returning
`Err(DbError::…)` for `release_advisory_lock` would cover both
production caller `tracing::warn!` arms via a single trait-substitute.
Same cost as NEW-R11-1 (add `tracing-subscriber` to dev-deps + capture
layer).

---

## 4. NEW: F1 warn-half (commits `fcf7ce3c` + `7c6bd2ec`)

Six sites pin a unified structured-warn shape:

```
app_id     = %app_id
audit_id   = id
transition = "Applied" | "Failed" | "Failed/invalid_index"
           | "Failed/data_violation" | "Failed/index_build"
           | (none — finalise_backfill uses terminal = ?terminal instead)
audit_err  = %audit_err   (NB: finalise_backfill uses `error = %e`)
```

Sites:

- `apply.rs:178-185` — Applied terminal.
- `apply.rs:209-217` — Failed terminal (DDL Err path).
- `backend/postgres.rs:496-503` — INVALID-index retry loop.
- `backend/postgres.rs:548-556` — data-violation retry.
- `backend/postgres.rs:597-606` — transient/non-transient index build.
- `migrations.rs:647-657` — `finalise_backfill` warn (still uses
  `error = %e` not `audit_err`; the unification is **5/6, not 6/6**;
  also includes `name`/`collection` per the cycle 12:47 fix).

### 4.1 The shape-drift gap

`7c6bd2ec`'s commit message claims "ONE shape across all 5" + "Pinned
… across all 5 F1 sites" — but adds `name`/`collection` to
`finalise_backfill` warn at `migrations.rs:647-657` and presents it as
the "6th site". Inspecting the source shows that 6th site still uses
`error = %e` (not `audit_err = %audit_err`), and uses `terminal =
?terminal` in place of `transition = "Applied|Failed|…"`. So:

- The 5 `update_audit_status` sites are unified (Applied / Failed /
  Failed/invalid_index / Failed/data_violation / Failed/index_build).
- The `finalise_backfill` warn is *adjacent in style* but not field-name-
  identical (`error` vs `audit_err`, `terminal` vs `transition`).

**This is the exact category of drift a snapshot/regex test would
catch.** A future refactor that touches one of the 5 unified sites and
forgets the others would land silently — `cargo test --lib` is green
either way. **Finding: NEW-R12-2 (LOW)** — no field-shape snapshot
test exists.

### 4.2 The event-fired gap

Same as NEW-R11-1, now across 7 new emission sites (2 from I6 + 5
from F1) plus the 2 from r11 (`5d9acab8`) = **9 untested tracing
emissions across 3 cycles**. Pattern is now established, not anecdotal.

---

## 5. Actionable test pattern for tracing emissions

Workspace already provides `tracing-subscriber` (`Cargo.toml:125`,
already a dep of `crates/core`). Cost to enable for plugin-db:

```
[dev-dependencies]
tracing-subscriber = { workspace = true }
```

Capture pattern (paraphrased; do NOT write the test, just naming the
shape):

```
let buf = Arc::new(Mutex::new(Vec::<String>::new()));
let layer = /* custom Layer or fmt::layer().with_writer(buf-writer) */;
let subscriber = tracing_subscriber::registry().with(layer);
tracing::subscriber::with_default(subscriber, || {
    // Exercise code path that should emit the warn.
});
let logs = buf.lock().unwrap();
assert!(logs.iter().any(|l|
    l.contains("update_audit_status failed") &&
    l.contains("transition=\"Failed/invalid_index\"") &&
    l.contains("audit_err=")
));
```

Three things this catches:

1. The warn fires at all (regression: someone deletes the `if let
   Err` arm).
2. The structured field names are stable (regression: someone renames
   `audit_err` back to `error` and breaks operator-side log filters).
3. The `transition` discriminator is present and matches the call
   site (regression: someone copy-pastes a warn and forgets to update
   the literal).

Same `tracing-subscriber` + capture-layer pattern covers all 9
emission sites with ~9 small tests. **Mechanical cost is small;
defensive value is real because operators grep these fields.**

---

## 6. Per-file `#[test]` count (HEAD = 81226451)

```
tests   file                            Δ vs r11
 149    src/query.rs                    0
  34    src/context.rs                  0
  29    src/broker.rs                   0
  26    src/wal_consumer.rs             0
  20    src/read_set.rs                 0
  16    src/auth/session.rs             0   (hardening-gated)
  14    src/replication.rs              0
  14    src/error.rs                    +1   (wasn't counted in r11; recount)
  11    src/diff.rs                     0
  10    src/v8_classes/replication.rs   0
   8    src/v8_classes/migration.rs     0
   6    src/orchestrator/lock_guard.rs  0
   6    src/exec.rs                     +3   ← ae5570dc
   4    src/replication_ops.rs          0
   4    src/orchestrator/auto_tx.rs     0
   4    src/auth/keys.rs                0   (gated)
   4    src/auth/bootstrap.rs           0   (gated)
   4    src/audit.rs                    0
   3    src/v8_classes/db.rs            0
   3    src/orchestrator/register_model/apply.rs   0   (3 unchanged despite +5 warns)
   3    src/migrations.rs               0   (3 unchanged despite +2 warns)
   2    src/v8_classes/subscription.rs  0
   2    src/backend/postgres.rs         0   (2 unchanged despite +3 warns; still source-grep + trait-bound)
   2    src/backend/mod.rs              0
```

Total default = 352 (matches). Total hardening = 352 + 24 = 376
(matches). The +3 net all lands in `exec.rs`. **No new test arrived
for any of the 3 files (`apply.rs`, `migrations.rs`, `postgres.rs`)
that received warn emissions this cycle.**

---

## 7. New gaps introduced this cycle

- **NEW-R12-1** (LOW) — I6 trait change spawned 2 new `tracing::warn!`
  caller branches with zero unit-test coverage of the `Err` path. No
  mock `Backend` impl exists in the crate. §3.
- **NEW-R12-2** (LOW) — 6-site unified F1 warn shape (`fcf7ce3c` +
  `7c6bd2ec`) has no snapshot/regex test pinning the structured field
  names. The `finalise_backfill` warn at `migrations.rs:647-657`
  already drifts from the 5-site unified shape (`error` vs
  `audit_err`, `terminal` vs `transition`) — a snapshot test would
  have flagged this at commit time. §4.1.
- **NEW-R11-1 carries + grows** — tracing-emission tests still absent;
  the gap is now 9 sites (2 from r11 + 7 from r12), not 2.

Severity: all LOW (observability-only, not data-path).

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
r12    86     +1     GAP-2/I13 CLOSED (testable subset, +3 tests in
                     exec.rs). Offset by 9-site tracing-emission gap +
                     2 new caller-Err arms + 6-site warn-shape drift,
                     all untested.
```

**Score: 86** — second consecutive +1.

### Why not higher

- 9 untested tracing emission sites across 3 cycles (NEW-R11-1 grew).
- I6 `Err` branches in `migrations.rs` have no unit tests; no mock
  `Backend` to enable them cheaply.
- F1 warn-shape unification claims "6 sites" but is 5+1 with two
  field-name differences in the 6th — exactly the drift a snapshot
  test would catch.
- GAP-3 (lenient strictness) still open (6th round).
- 4 bare files unchanged; `v8_bridge.rs` now has 2 production
  `tracing::warn!` lines and still 0 tests.

### Why not lower

- GAP-2 (I13) genuinely closes; 3 tightly-written unit tests cover 3
  of 4 branches, the 4th is correctly delegated to integration.
- The 3 new exec tests assert real broker subscription behaviour (real
  `broker::subscribe`, `pop()`, `Change` event payload), not
  implementation details. Good coverage shape.
- The cycle's two doc-drift commits (`71a457a1`) are code-neutral.
- 352/376 lib tests all green in <0.2s.
- The unified warn shape is *partly* a quality improvement (5 of 6
  sites converged), even though the test-side hasn't caught up.

---

## 9. Files reviewed

- `/home/ruiyang/Projects/appbase/crates/plugin-db/Cargo.toml`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/exec.rs:450-590`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs:280-300, 640-680`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/postgres.rs:157-220, 480-610, 700-766`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/backend/mod.rs:155-210`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs:155-220`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs:362-412`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs:320-360`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/tests/integration.rs` (grep audit only)
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-test-coverage-2026-05-22-r11.md`
- `/home/ruiyang/Projects/appbase/docs/reviews/plugin-db-deferred.md`
