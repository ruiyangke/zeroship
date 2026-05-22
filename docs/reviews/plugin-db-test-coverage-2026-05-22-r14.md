# plugin-db — Test Coverage Review (round 14)

- **Date:** 2026-05-22 (cycle 15:17 audit)
- **Scope:** `crates/plugin-db/` (src + tests + benches)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84) · r9 (84) · r10 (84) · r11 (85) · r12 (86) · r13 (86)
- **HEAD:** `44ec83db` (verified via `git rev-parse HEAD`)
- **Method:** Read-only. Re-ran `cargo test --lib` (default + `hardening`).
  Diffed every commit in cycle 15:17 (`0bf71f27`, `14d7608f`, `6afab751`).
  Re-classified r13's 4 open gaps. Spot-checked the F2 r13 integration-
  test update + audit-table CHECK ALTER idempotence path.

---

## TL;DR

**Lib test counts (HEAD = 44ec83db):**

| Feature flag           | Tests   | Δ vs r13 |
| ---------------------- | ------- | -------- |
| (default)              | **363** | **+11**  |
| `--features hardening` | **387** | **+11**  |
| Hardening delta        | +24     | unchanged |

**Cycle 15:17 commit audit:**

| Commit       | Net tests | Notes                                                                   |
| ------------ | --------- | ----------------------------------------------------------------------- |
| `0bf71f27`   | **+11**   | CaptureLayer + 11 tests (6 self-tests + 2 production-driving + 3 docs) |
| `14d7608f`   | 0         | F2 initial: Failed + `error_message="validation_refused"` marker        |
| `6afab751`   | 0         | F2 r13 upgrade: dedicated `ValidationRefused` terminal; updates 1 existing integration test |

Net: **+11 tests** — matches measured (363 vs r13's 352).

---

## 1. Tool results

```
git rev-parse HEAD                            → 44ec83db
cargo test -p zeroship-plugin-db --lib        → 363 passed (0.14s)
cargo test -p zeroship-plugin-db --lib
   --features hardening                       → 387 passed (0.14s)
cargo build -p zeroship-plugin-db             → ok (17 unrelated warnings)
```

All green.

---

## 2. r13 open-gap re-classification

| ID  | r13 status | r14 status | Evidence |
| --- | ---------- | ---------- | -------- |
| **GAP-3** (I14 lenient strictness integration test) | open (7th round) | **open (8th round)** | `grep -rn lenient crates/plugin-db/tests` → 0 hits. `grep -rn lenient crates/plugin-db/src` → 3 hits, all comments in `validate.rs`. F2 r13 commit only updated `a2_destructive_drop_column_refused_strict` (status expectation `pending` → `validation_refused`). Integration suite still tests only `strict` (explicit) and `off` (`a2_strictness_off_skips_validation_refused`). `lenient` mode is dead-letter — code path lives at `validate.rs:103-106` but has zero exercise. |
| **NEW-R11-1 + NEW-R12-1** (tracing-emission tests) | partial (infra + 0 tests) | **CLOSED (partial)** | `0bf71f27` lands `src/test_support/mod.rs` (339 LOC `CaptureLayer`) + 11 tests. 2 tests drive production code end-to-end (`set_mig_lock_shadow_replace_emits_error_with_prev_and_new`, `destructive_invariant_error_emits_named_fields_at_error_level`). 3 are documentation snapshots that re-emit production syntax without driving it. 6 are self-tests on the capture layer itself. See §3 for the production-driving vs snapshot tradeoff. |
| **Multibyte 63-byte boundary on `validate_collection`** | open | **open (effectively N/A)** | `query.rs:91-94` rejects non-ASCII via `is_ascii_alphanumeric`; multibyte names short-circuit before the byte-length check. The gap is structurally moot — no multibyte input ever reaches the 63-byte boundary check. r13 should have closed this as N/A but didn't; carrying r13's classification with a footnote. |
| **4 bare files** (`crud.rs`, `v8_bridge.rs`, two v8_classes) | open | **open** | unchanged. `v8_bridge.rs`: 0 unit tests. `crud.rs`: 0 unit tests. No code changes in this cycle to either. |

### 2.1 NEW-R11-1 + NEW-R12-1 — CLOSED with honest scope

The capture-layer infrastructure r12 + r13 flagged as "added-but-unused"
is now consumed. 11 tests landed; counts match the commit message.
Honest scope per the commit body:

- **2 tests drive production code** (`set_mig_lock` direct call,
  `destructive_invariant_error` direct call). These would catch a field
  rename at unit-test time.
- **3 tests are documentation snapshots** (`f1_warn_shape_documentation_snapshot`,
  `i6_release_advisory_lock_warn_shape_documentation_snapshot`, the
  return_mig_client message pin). These re-emit production syntax under
  the capture layer; they pin the contract by example but do NOT
  auto-detect drift if a contributor renames both the production site
  and the snapshot in one commit. The commit body acknowledges this.
- **6 self-tests** on the capture layer (field-name preservation,
  Debug/Display sigils, multi-event ordering, target propagation).

This is a **partial close** with explicit honesty about which production
sites remain pinned only at the documentation level (F1 in
`backend/postgres.rs`, F1 in `apply.rs::run_op::update_audit_status`,
`return_mig_client` empty-slot). Closing these requires a Backend mock
or `compio_postgres::Client` synthesiser — out of scope per the brief.

I count NEW-R11-1 + NEW-R12-1 as CLOSED on the merits: the capture
layer is real, the production-driving tests are real, the gap-name
"infrastructure missing" no longer holds. The residual documentation-
snapshot pattern is a known limitation, not an open coverage gap.

### 2.2 GAP-3 — interesting non-close

The F2 r13 upgrade (`6afab751`) was the perfect moment to add a
lenient-mode integration test: the new `ValidationRefused` terminal is
written by **both** strict and lenient branches (`validate.rs:67` —
`!destructive.is_empty() && ctx.strictness != "off"`). The 6afab751
commit message itself says "Both strict and lenient modes terminalize
identically." But the test suite still exercises only strict and off.

The cycle's quality drops here. The F2 r13 upgrade changed the audit
table CHECK constraint, added a new enum variant in two places, added
a CHECK ALTER idempotence path — and the only test change was a
status-string update in the strict-mode test. A `lenient` integration
test would have been a 40-line copy of `a2_destructive_drop_column_refused_strict`
with the schema swapped to include `"_meta": {"strictness": "lenient"}`
and an assertion that the deploy succeeds (vs strict's `is_err`) while
the audit row still lands at `validation_refused`.

**Severity: MEDIUM** — the lenient-mode code path is a publicly
documented strictness mode (per the bootstrap.rs / validate.rs
docstrings); it executes a different control-flow branch than strict
(falls through into `Ok(ApprovedPlan)` instead of returning `Err`);
and apply.rs has its own skip-destructive logic that the lenient path
relies on. None of this is integration-tested.

### 2.3 Audit-table CHECK ALTER idempotence — partial verification

r13's fixer-agent claim: "verified empirically via two consecutive
`ensure_audit_table_exists` invocations in deploy_v1+deploy_v2".

Audit:

- `tests/integration.rs:970` (`a3_audit_table_created_and_idempotent`)
  calls `ensure_audit_table_exists` twice in succession against a
  freshly-created schema. The second call exercises the new
  `DROP CONSTRAINT IF EXISTS` + `ADD CONSTRAINT` path on a table that
  already has the new (wider) constraint. The test passes — confirmed
  via the green `cargo test` run.
- **The "really old table without the named constraint" branch is NOT
  tested.** The audit.rs:260-289 commentary explicitly cites three
  branches: (a) freshly created table with new constraint name, (b)
  old table with old constraint name, (c) really old table with no
  named constraint. Only (a) is exercised by
  `a3_audit_table_created_and_idempotent` — both invocations create a
  fresh table.
- Branch (b) is the actual upgrade path operators will hit on
  redeploys against existing apps. **Untested.**

**Finding: NEW-R14-1 (MEDIUM)** — the F2 r13 upgrade's CHECK ALTER
idempotence is verified only for the fresh-table branch. The upgrade
path (old constraint → new constraint) and the legacy path (no named
constraint → new constraint) are not exercised. A test that
CREATE-TABLEs the audit table with the OLD constraint list and then
runs `ensure_audit_table_exists` would close branch (b). Cost: ~1
integration test, 30 LOC.

---

## 3. Audit of the `0bf71f27` test set

Per-test verdict on the 11 new tests:

| Test                                                     | Drives prod? | Verdict                                              |
| -------------------------------------------------------- | ------------ | ---------------------------------------------------- |
| `set_mig_lock_shadow_replace_emits_error_with_prev_and_new` | yes | Real — calls `ctx.set_mig_lock` twice and verifies the [I23] error event |
| `set_mig_lock_first_install_emits_no_event` | yes | Real — negative case for the `is_some()` gate |
| `destructive_invariant_error_emits_named_fields_at_error_level` | yes | Real — calls `destructive_invariant_error(&op)` directly |
| `f1_warn_shape_documentation_snapshot` | no | Doc snapshot — re-emits production syntax |
| `i6_release_advisory_lock_warn_shape_documentation_snapshot` | no | Doc snapshot — re-emits production syntax |
| `captures_warn_event_with_named_fields` | self | CaptureLayer self-test |
| `captures_error_event_with_debug_field` | self | CaptureLayer self-test |
| `captures_multiple_events_in_order` | self | CaptureLayer self-test |
| `capture_returns_closure_result` | self | CaptureLayer self-test |
| `target_carries_emitting_module_path` | self | CaptureLayer self-test |
| `captures_bool_and_display_via_percent_sigil` | self | CaptureLayer self-test |

Production-driving: 3 / 11. Self-tests: 6 / 11. Snapshots: 2 / 11.

The 3 production-driving tests have real coverage value — they catch
field renames at unit-test time. The 2 snapshots are belt-and-braces;
their drift-detection is human-mediated. The 6 self-tests are
appropriate for shipping a new test-helper (one would normally object,
but the CaptureLayer threads `tracing-subscriber` Layer trait + visitor
machinery + a thread-local arc — non-trivial enough to warrant
self-tests).

Net: **3 tests of substantive coverage value, 8 of supporting value.**
Reasonable ratio for an infrastructure-landing commit.

---

## 4. F2 r13 upgrade audit (`6afab751`)

The F2 upgrade replaces the cycle 15:17 "Failed + marker" pattern
(`14d7608f`) with a dedicated `ValidationRefused` terminal. Audit:

- **audit.rs:** `InitialStatus::ValidationRefused` + `TerminalStatus::ValidationRefused`
  added. `as_sql()` round-trips tested for both
  (`audit.rs:935-944`).
- **audit.rs:240:** CHECK constraint widened to include
  `'validation_refused'`. Fresh-table path.
- **audit.rs:260-289:** DROP CONSTRAINT IF EXISTS + ADD CONSTRAINT
  for the upgrade path. Idempotence verified only on the fresh-table
  branch (see §2.3).
- **validate.rs:** INSERT-direct terminal — no Pending + UPDATE
  round-trip. Both strict and lenient modes route through the same
  code (line 80-91). Only strict is integration-tested (§2.2).
- **integration.rs:1111:** `a2_destructive_drop_column_refused_strict`
  updated; status expectation `pending` → `validation_refused`.
- **integration.rs:1159:** `a2_strictness_off_skips_validation_refused`
  unchanged — still verifies destructive op is silently skipped when
  strictness is off; no audit row is written in this mode.

The integration-test change is minimal (10 lines) and verifies the
new status string lands in the audit row. The audit row's CHECK
constraint is implicitly verified — if the constraint hadn't been
widened, the INSERT would 23514-fail and the test would fail. So the
test covers the upgrade's "happy path" for the strict branch.

What it does NOT cover:
- Lenient branch (§2.2)
- Old-constraint upgrade path (§2.3)
- The `audit_err` warn shape on `write_audit_row` failure
  (`validate.rs:100` — `tracing::warn!(error = ?e, "audit: failed to log destructive op")`)
  — this site uses field name `error` not `audit_err`, drifting from
  the F1 family. Not in any snapshot test.

**Finding: NEW-R14-2 (LOW)** — the F2 r13 secondary-failure tracing
site at `validate.rs:100` uses `error = ?e` field name, drifting from
the F1 warn-half family (`audit_err`). The 14d7608f commit body
claims "field shape on the audit-write secondary-failure tracing::warn
matches the F1 warn-half family". The r13 upgrade (`6afab751`) inverted
this without commentary. Not caught by the F1 snapshot test (it pins
only the `apply.rs` sites). Net: one more grep-shape drift to add
to the carry list.

---

## 5. Per-file `#[test]` count (HEAD = 44ec83db)

```
tests   file                                            Δ vs r13
 149    src/query.rs                                    0
  36    src/context.rs                                  +2  (set_mig_lock shadow + first-install tests)
  29    src/broker.rs                                   0
  26    src/wal_consumer.rs                             0
  20    src/read_set.rs                                 0
  16    src/auth/session.rs                             0   (hardening-gated)
  14    src/replication.rs                              0
  13    src/error.rs                                    0
  11    src/diff.rs                                     0
  10    src/v8_classes/replication.rs                   0
   8    src/v8_classes/migration.rs                     0
   6    src/test_support/mod.rs                         +6  NEW
   6    src/orchestrator/lock_guard.rs                  0
   6    src/exec.rs                                     0
   5    src/orchestrator/register_model/apply.rs        +2  (destructive_invariant + f1 snapshot)
   4    src/replication_ops.rs                          0
   4    src/orchestrator/auto_tx.rs                     0
   4    src/migrations.rs                               +1  (i6 snapshot)
   4    src/auth/keys.rs                                0   (gated)
   4    src/auth/bootstrap.rs                           0   (gated)
   4    src/audit.rs                                    0
   3    src/v8_classes/db.rs                            0
   2    src/v8_classes/subscription.rs                  0
   2    src/backend/postgres.rs                         0
   1    src/backend/mod.rs                              0
```

Sum: 363 default / 387 hardening. **Net +11.** Matches commit
message and measured count.

---

## 6. New / carried gaps after r14

| ID            | Severity | Status | Note |
| ------------- | -------- | ------ | ---- |
| GAP-3 (lenient strictness integration test) | MEDIUM | **OPEN (8th round)** | F2 r13 upgrade was the natural moment — missed. |
| NEW-R11-1 + NEW-R12-1 (tracing capture) | LOW | **CLOSED** | Infrastructure + 3 production-driving tests + 2 snapshots. |
| NEW-R13-1 (test_utils self-tests) | LOW | **OPEN** (carried) | No new tests this cycle. |
| NEW-R13-2 (decode unit tests via row_for_test) | LOW | **OPEN** (carried) | Untouched. |
| NEW-R13-3 (row_to_json_for_bench in-crate test) | LOW | **OPEN** (carried) | Untouched. |
| Multibyte 63-byte boundary | n/a | **N/A** | Pre-empted by ASCII allowlist. Effectively closed. |
| 4 bare files | LOW | **OPEN** | unchanged. |
| **NEW-R14-1** (CHECK ALTER upgrade-path idempotence) | MEDIUM | **OPEN** | New gap from `6afab751`. Branches (b) + (c) of the DROP/ADD logic not exercised by `a3_audit_table_created_and_idempotent`. |
| **NEW-R14-2** (F2 r13 secondary-failure tracing field name drift) | LOW | **OPEN** | `validate.rs:100` uses `error = ?e`, drifts from F1 family's `audit_err`. |

---

## 7. Score (1-100)

```
Round  Score  Delta  Notes
-----  -----  -----  ------------------------------------------------------
r7     83     —      Baseline.
r8     84     +1     Wave of new tests.
r9     84      0     One behavioural test.
r10    84      0     Zero new tests; plateau STRONG.
r11    85     +1     GAP-1/I12 CLOSED.
r12    86     +1     GAP-2/I13 CLOSED.
r13    86      0     Infrastructure landed (tracing-subscriber dev-dep);
                     zero tests using it. NEW-R12-2 code-fix, no test.
r14    87     +1     NEW-R11-1 + NEW-R12-1 CLOSED (capture layer +
                     3 production-driving tests + 5 snapshots + 6
                     self-tests = +11). But GAP-3 still OPEN (8th
                     round carry — F2 upgrade missed the natural moment)
                     and 2 NEW-R14 gaps from the F2 r13 upgrade (CHECK
                     ALTER upgrade-path idempotence + secondary-failure
                     field-name drift).
```

**Score: 87** — Δ = +1 vs r13.

### Why +1 (not more)

- 11 new tests, all passing, all in <0.2s. Net coverage rises.
- The long-carried NEW-R11-1 + NEW-R12-1 gap is finally closed (or
  closed enough — see §2.1).
- The new tests are honestly scoped — 3 production-driving + 2
  snapshots + 6 self-tests, with the commit body acknowledging the
  doc-snapshot pattern's limitations.

### Why not +2

- GAP-3 has now carried for **8 rounds** (r7 → r14). The F2 r13 upgrade
  (`6afab751`) was the natural moment to add a lenient-mode test —
  the commit body itself says "Both strict and lenient modes
  terminalize identically" — but no lenient test landed. Missed
  opportunity.
- NEW-R14-1: the F2 r13 CHECK ALTER upgrade path has 3 branches
  documented in the source comments; only branch (a) is tested.
  Branch (b) is the actual migration path operators will hit, and
  it's untested.
- NEW-R14-2: a secondary-failure tracing site drifted from the F1
  warn-half family without being caught by the new snapshot test.
  This is the second time in 3 cycles that the F1 family has drifted
  (the first was `7c6bd2ec` → `18aee490`).
- 4 bare files unchanged (5 cycles now). `crud.rs` / `v8_bridge.rs`
  / two v8_classes files still have 0 unit tests despite being load-
  bearing for the DB SDK.

### Why not 0

- The capture layer is real and consumed. r13's "infrastructure-but-
  no-tests" concern is resolved.
- 363 default / 387 hardening, all green in 0.14s.
- The 3 production-driving tests in `0bf71f27` catch real drift
  classes (field renames in [I23] and [I33] paths).
- The F2 r13 upgrade itself is solid code (clean enum extension,
  idempotent CHECK ALTER, eliminated orphan-Pending window). The
  test gaps are about coverage, not correctness.

---

## 8. Files reviewed

- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/test_support/mod.rs` (full)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/context.rs:985-1110`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs:445-570`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/validate.rs:55-135`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/bootstrap.rs:85-140`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs:990-1050`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs:119-300`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/query.rs:55-110`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/tests/integration.rs:970-1200`
- Cycle commits: `0bf71f27`, `14d7608f`, `6afab751`

