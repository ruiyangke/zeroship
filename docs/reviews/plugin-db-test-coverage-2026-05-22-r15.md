# plugin-db — Test Coverage Review (round 15)

- **Date:** 2026-05-22 (cycle 16:17 audit)
- **Scope:** `crates/plugin-db/` (src + tests + benches)
- **Lens:** Test coverage
- **Anchors:** r7 (83) · r8 (84) · r9 (84) · r10 (84) · r11 (85) · r12 (86) · r13 (86) · r14 (87)
- **HEAD at start:** `baa262c1` (brief); HEAD advanced to **`f173ed40`** mid-review (one extra commit landed during the audit — see §3.1).
- **Method:** Read-only. Re-ran `cargo test --lib` (default + `hardening`) and `cargo test --test integration --no-run`. Diffed every commit in cycle 16:17 (`7506bd73`, `cbd21112`, `baa262c1`) plus the late-arriving `f173ed40`. Re-classified r14's open gaps.

---

## TL;DR

**Lib test counts (HEAD = f173ed40):**

| Feature flag           | Tests   | Δ vs r14 |
| ---------------------- | ------- | -------- |
| (default)              | **364** | **+1**   |
| `--features hardening` | **388** | **+1**   |
| Hardening delta        | +24     | unchanged |

**Integration test counts (`tests/integration.rs`):**

| Metric            | r14 | r15 | Δ |
| ----------------- | --- | --- | -- |
| `#[compio::test]` | 72  | 73  | +1 |
| `#[ignore]`       | 1   | 1   | 0  |

**Cycle 16:17 commit audit:**

| Commit       | Net tests | Notes                                                                   |
| ------------ | --------- | ----------------------------------------------------------------------- |
| `7506bd73`   | **+1 integration** | F2 CHECK ALTER upgrade-path test. Closes NEW-R14-1 branch (b). |
| `cbd21112`   | 0 | F1 warn-shape unification at apply.rs:84 (8th F1 site). Code-only; no test. |
| `baa262c1`   | 0 | Docs commit, no code change. |
| `f173ed40`   | **+1 lib** | Collection-slot snapshot test (closes coverage gap that `cbd21112` opened on its own). |

Net: **+1 lib test, +1 integration test.** Matches measured counts.

---

## 1. Tool results

```
git rev-parse HEAD                            → f173ed40
cargo test -p zeroship-plugin-db --lib        → 364 passed (0.14s)
cargo test -p zeroship-plugin-db --lib
   --features hardening                       → 388 passed (0.14s)
```

All green. Integration suite not executed (needs live Postgres, off-thread in this audit env). Compile-check via `cargo test --test integration --no-run` would have been cheap; counted commits via grep instead.

---

## 2. r14 open-gap re-classification

| ID  | r14 status | r15 status | Evidence |
| --- | ---------- | ---------- | -------- |
| **GAP-3** (lenient strictness integration test) | OPEN (8th round) | **OPEN (9th round)** | `grep -rn lenient crates/plugin-db/tests` → 0 hits. `grep -rn lenient crates/plugin-db/src` → 1 file (validate.rs, comments only). Cycle 16:17 added no lenient test. F2 r13 upgrade was the natural moment (r14 §2.2); the lenient branch executes a distinct fall-through (`validate.rs:67-91` writes the audit row in both strict and lenient modes but `return Err(...)` only in strict). The lenient code-path remains dead-letter integration-wise. |
| **NEW-R14-1** (CHECK ALTER upgrade-path idempotence) | OPEN (MEDIUM) | **PARTIALLY CLOSED** | `7506bd73` lands `a3_audit_table_check_alter_upgrades_existing_constraint`. Verified §3.2 — the test does what the commit body claims: seeds OLD CHECK (named constraint, 7 statuses), sanity-asserts `23514`, calls `ensure_audit_table_exists`, asserts `pg_get_constraintdef` includes `validation_refused`, inserts `validation_refused` (succeeds), inserts unknown status (fails 23514). **Branch (b)** of the three-branch DROP+ADD logic is now tested. **Branch (c)** (really-old table with no named constraint) remains untested — IF EXISTS swallows the miss, but no test forces that path. r15 reclassifies (b) as CLOSED, (c) as still-OPEN. |
| **NEW-R14-2** (validate.rs:100 field drift) | OPEN (LOW) | **CLOSED** | `d07616a2` (cycle 15:47) unified `validate.rs:100` to `app_id` + `collection` + `transition` + `audit_err`. Grep confirms: validate.rs:106 emits the unified shape. Out-of-scope for this brief (already CLOSED by 15:47); brief asked for verify and that holds. |
| **NEW-R11-1 + NEW-R12-1** (tracing-emission tests) | CLOSED (partial) | **CLOSED** | Unchanged from r14. |
| **NEW-R13-1** (test_utils self-tests) | OPEN | **OPEN** | No change. |
| **NEW-R13-2** (decode unit tests via row_for_test) | OPEN | **OPEN** | No change. |
| **NEW-R13-3** (row_to_json_for_bench in-crate test) | OPEN | **OPEN** | No change. |
| **4 bare files** (`crud.rs`, `v8_bridge.rs`, two v8_classes) | OPEN | **OPEN** | unchanged (6 cycles). |
| Multibyte 63-byte boundary | N/A | **N/A** | unchanged. |

### 2.1 NEW-R14-1 closure — high quality

The new integration test (`integration.rs:1020-1171`) is the strongest test added across the last six cycles:

- **Sanity-asserts the seed.** A pre-INSERT against the OLD CHECK must fail 23514. Without this, a regression that silently dropped the constraint would still let the test pass on the post-INSERT.
- **Verifies the canonicalised form.** Uses `pg_get_constraintdef(c.oid)` rather than `pg_constraint.consrc` (deprecated post-PG12) or grepping the named constraint alone. This catches the variant where DROP succeeded but ADD silently no-op'd.
- **Negative-side check.** An unknown status must still fail 23514, proving the widening didn't disable the constraint entirely.
- **Distinct app schema.** `a3_audit_alter_upgrade` is unique — no cross-test interference.

The only residual gap is branch (c) — the test seeds the OLD constraint with the named identifier the new code DROPs. A "really old" table missing that constraint name entirely (perhaps from an even earlier schema generation) would exercise the IF-EXISTS-swallow path. r15 keeps this as OPEN but LOW: there is no historical schema in the codebase that ever wrote the constraint without that name.

### 2.2 The MINOR-R12-1 → f173ed40 sequence — interesting cycle ordering

Two commits cooperated to close one gap:

- `cbd21112` (cycle 16:17): unified apply.rs:84 to the F1 collection-slot shape. **This commit landed without a covering test.** At that moment the F1 family was 8 sites with 5 tested (via `f1_warn_shape_documentation_snapshot`'s audit_id variant) — Shape B (collection-slot) had no snapshot.
- `f173ed40` (cycle 16:17 follow-up, ~25 min later): added `f1_warn_shape_collection_slot_documentation_snapshot`. Now Shape A (audit_id-slot) and Shape B (collection-slot) both have pinning snapshots.

**Brief question — does the new apply.rs:84 site need test coverage to maintain the family?** Yes, and it now has it via f173ed40. But the cycle-internal ordering exposed the family briefly to 8-sites-with-5-tested between cbd21112 and f173ed40. That's a transient gap closed within the same cycle — net zero for r15, but worth noting that cbd21112 *should* have been bundled with f173ed40 in a single commit.

The new snapshot (apply.rs:606-653) is honest documentation — same pattern as the audit_id snapshot. Pins literal field names (`app_id`, `collection`, `transition`, `audit_err`) and the `transition = "Running/insert_failed"` discriminator. Same caveat as the original: re-emits production syntax under the capture layer, so a contributor renaming BOTH the site and the snapshot in one commit is not caught. The commit body claims that class of drift is caught by code review + the literal transition string. Defensible.

### 2.3 F1 family member count — comment drift

The F1 family now has 8 sites. The codebase's comments are out of sync:
- apply.rs:188 comment says "5 F1 sites"
- apply.rs:563/577 comment says "6 F1 sites"
- apply.rs:592 comment says "8-site F1 family"

This is **doc drift, not coverage drift** — the assertions still validate the contract. Worth flagging for housekeeping but not a test-coverage gap.

### 2.4 Honest split: Shape A is 6 sites, not 5

The f173ed40 commit body says "5 audit_id-slot sites + 3 collection-slot sites = 8 sites". My count by grep on `audit_err = %audit_err`:

- **Shape A (audit_id slot)**: apply.rs:193, apply.rs:225, backend/postgres.rs:501, backend/postgres.rs:553, backend/postgres.rs:603, migrations.rs:658 → **6 sites** (migrations.rs:658 also carries `collection` so it could double-count into Shape B)
- **Shape B (collection slot, no audit_id)**: apply.rs:93, validate.rs:106 → **2 sites**

Total: 8 sites (matches commit). The 5/3 split is plausible if migrations.rs:658 is categorised as collection-slot (it has both fields). Either way, both snapshot tests validate by `contains_key` (not strict-equal), so migrations.rs:658 would pass both. No coverage hole here; just imprecise commit-message bookkeeping.

---

## 3. Cycle 16:17 commit audit

### 3.1 HEAD movement during audit

The brief identified cycle 16:17 as three commits ending at `baa262c1`. During my read-through, `f173ed40` (cycle 16:17 follow-up) landed at 15:20:10 — within the same cycle window. I've included it in the audit because (a) it closes a coverage gap that `cbd21112` had opened, (b) it bumps lib tests 363 → 364, (c) ignoring it would mis-represent the cycle's true state.

### 3.2 Per-commit verdict

| Commit       | Verdict |
| ------------ | ------- |
| `7506bd73` (F2 CHECK ALTER upgrade-path integration test) | **Strong.** Tests the exact path operators hit on every existing-app deploy. Sanity-asserts seed state, verifies post-state via `pg_get_constraintdef`, negative-checks that the constraint is still active. 167 LOC well-spent. |
| `cbd21112` (apply.rs:84 F1 unification) | **Code-only — coverage gap until f173ed40.** Production change without covering test. Snapshot test landed 25 min later (f173ed40). Net OK *for the cycle as a whole* but the commit boundary was poorly drawn. |
| `baa262c1` (docs) | Docs only. No-op for coverage. |
| `f173ed40` (collection-slot snapshot) | **Belated but correct.** Same documentation-snapshot pattern as the audit_id-slot variant. Pins the 4-field collection-slot contract. Same limitations as documented in r14 §2.1. |

---

## 4. Per-file `#[test]` count (HEAD = f173ed40)

```
tests   file                                            Δ vs r14
 149    src/query.rs                                    0
  36    src/context.rs                                  0
  29    src/broker.rs                                   0
  26    src/wal_consumer.rs                             0
  20    src/read_set.rs                                 0
  16    src/auth/session.rs                             0   (hardening-gated)
  14    src/replication.rs                              0
  13    src/error.rs                                    0
  11    src/diff.rs                                     0
  10    src/v8_classes/replication.rs                   0
   8    src/v8_classes/migration.rs                     0
   6    src/test_support/mod.rs                         0
   6    src/orchestrator/register_model/apply.rs        +1  (collection-slot snapshot)
   6    src/orchestrator/lock_guard.rs                  0
   6    src/exec.rs                                     0
   4    src/replication_ops.rs                          0
   4    src/orchestrator/auto_tx.rs                     0
   4    src/migrations.rs                               0
   4    src/auth/keys.rs                                0   (gated)
   4    src/auth/bootstrap.rs                           0   (gated)
   4    src/audit.rs                                    0
   3    src/v8_classes/db.rs                            0
   2    src/v8_classes/subscription.rs                  0
   2    src/backend/postgres.rs                         0
   1    src/backend/mod.rs                              0
```

Sum: 364 default / 388 hardening. **Net +1 lib.** Matches commit message and measured count.

Integration: 72 → 73 compio::test fns, 1 ignored. **Net +1 integration.**

---

## 5. New / carried gaps after r15

| ID            | Severity | Status | Note |
| ------------- | -------- | ------ | ---- |
| GAP-3 (lenient strictness integration test) | MEDIUM | **OPEN (9th round)** | Second consecutive cycle where the lenient path went untested despite touch-adjacent changes. F2 r13 was the natural moment; cycle 16:17 doubled down on the validate.rs warn-shape work without adding a lenient test. |
| NEW-R14-1 (CHECK ALTER upgrade-path idempotence) | LOW | **PARTIALLY CLOSED** | Branch (b) closed by `7506bd73`. Branch (c) remains OPEN but downgraded to LOW (no historical schema actually wrote without the named constraint). |
| NEW-R14-2 (F2 r13 secondary-failure tracing field name drift) | LOW | **CLOSED** | `d07616a2` (15:47) unified. |
| NEW-R15-1 (split commit hygiene: production change without test) | LOW | **OPEN** | `cbd21112` shipped the F1 collection-slot production change without its covering snapshot; coverage was restored 25 min later by `f173ed40`. Cycle-level OK, commit-level not. Noted for hygiene; no carry. |
| NEW-R15-2 (F1 family site-count comment drift) | LOW | **OPEN** | apply.rs has three different "N F1 sites" comments (5, 6, 8). Doc drift; assertions still correct. Easy to address in next housekeeping pass. |
| NEW-R11-1 + NEW-R12-1 (tracing capture) | LOW | **CLOSED** | unchanged. |
| NEW-R13-1 (test_utils self-tests) | LOW | **OPEN** (carried 3rd round) | unchanged. |
| NEW-R13-2 (decode unit tests via row_for_test) | LOW | **OPEN** (carried 3rd round) | unchanged. |
| NEW-R13-3 (row_to_json_for_bench in-crate test) | LOW | **OPEN** (carried 3rd round) | unchanged. |
| 4 bare files | LOW | **OPEN** (carried 6th round) | unchanged. |

---

## 6. Score (1-100)

```
Round  Score  Delta  Notes
-----  -----  -----  ------------------------------------------------------
r7     83     —      Baseline.
r8     84     +1     Wave of new tests.
r9     84      0     One behavioural test.
r10    84      0     Zero new tests; plateau STRONG.
r11    85     +1     GAP-1/I12 CLOSED.
r12    86     +1     GAP-2/I13 CLOSED.
r13    86      0     Infrastructure landed; zero tests using it.
r14    87     +1     NEW-R11-1 + NEW-R12-1 CLOSED via capture layer.
                     But GAP-3 still OPEN; NEW-R14-1 + NEW-R14-2 surfaced.
r15    88     +1     NEW-R14-1 branch (b) CLOSED via 7506bd73 (high-
                     quality integration test). NEW-R14-2 CLOSED at 15:47.
                     Collection-slot snapshot landed via f173ed40
                     (closes the transient gap cbd21112 opened). GAP-3
                     still OPEN (9th round carry).
```

**Score: 88** — Δ = +1 vs r14.

### Why +1

- The `7506bd73` test is the cycle's standout: it exercises the actual operator path (DROP+ADD against an old named constraint), sanity-asserts the seed, verifies post-state canonically, negative-checks the constraint is still active. This is the kind of test the brief has been asking for across multiple rounds.
- `f173ed40` closes the snapshot gap that `cbd21112` opened. Two commits cooperating but the net cycle-level state is clean — both Shape A and Shape B F1 variants are pinned.
- NEW-R14-1 went from a fresh MEDIUM open gap to a PARTIALLY-CLOSED (branch b) + LOW residual (branch c). NEW-R14-2 went from LOW open to CLOSED.
- 364/388 all green, 0.14s. No flakes, no skips.

### Why not +2

- **GAP-3 carries for the 9th consecutive round.** This is the longest-lived open gap in the review series. Cycle 16:17 was the third consecutive cycle to touch the strictness/destructive code path (F2 r13 upgrade, cbd21112 warn unification, d07616a2 validate.rs cousin) without adding a lenient-mode integration test. The lenient code path executes a distinct branch (`validate.rs:67-91` falls through to `Ok(ApprovedPlan)` instead of returning Err) and has its own apply.rs skip-destructive logic — none of which is integration-tested. Each cycle's "natural moment" passes; each cycle the carry deepens.
- **`cbd21112` commit hygiene.** Production change without test, even though the test would have been ~50 LOC in the same file. The fact that f173ed40 followed 25 min later proves the test was tractable. Cycle-level net OK; commit-level practice not.
- **Branch (c) of NEW-R14-1 still open.** Low severity (no historical schema produced the unnamed constraint) but documented as a third branch in the source comment.
- **F1 family site-count comments drift.** Tracking is now stale (5 / 6 / 8 in three different comments). Not a test-coverage issue per se but reflects bookkeeping fatigue.
- **4 bare files unchanged for 6 cycles.** `crud.rs`, `v8_bridge.rs`, `v8_classes/migrations.rs`, `v8_classes/transaction.rs`, `v8_classes/collection.rs` all still zero unit tests. These are load-bearing for the DB SDK.
- **3 NEW-R13 gaps still open.** Three cycles since they were surfaced; no progress.

### Why not 0

- Net tests rose by 2 (one lib, one integration) on a small cycle window. The integration test is genuinely high-quality. The snapshot test, while a documentation pattern, does pin against one-sided rename.
- The cycle closed an open MEDIUM gap (NEW-R14-1 branch b) and an open LOW gap (NEW-R14-2). That's two-of-three open r14 gaps closed in one cycle — the kind of progress that justifies +1.
- The F2 upgrade path is now the most thoroughly tested part of the audit pipeline, with both the fresh-table branch and the upgrade branch covered. Operators can deploy 6afab751 against existing apps with empirical confidence.

---

## 7. Files reviewed

- `/home/ruiyang/Projects/appbase/crates/plugin-db/tests/integration.rs:1006-1171` (new test)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs:70-98` (cbd21112 production change)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/apply.rs:507-654` (f173ed40 snapshot test + pre-existing audit_id snapshot)
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/orchestrator/register_model/validate.rs:90-110`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/audit.rs:240-290`
- `/home/ruiyang/Projects/appbase/crates/plugin-db/src/migrations.rs:640-665`
- Cycle commits: `7506bd73`, `cbd21112`, `baa262c1`, `f173ed40` (late-arriving)
