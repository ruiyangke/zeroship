# plugin-db: Migration / DDL Pipeline Correctness Review (R2)
**HEAD:** 3ef6a170 · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior round:** `plugin-db-migration-pipeline-2026-05-22-r1.md`

---

## 1. Pipeline Diagram (current HEAD)

```
register_model_dispatch                  (orchestrator/register_model/mod.rs:63)
        │
        ├─ [fast path] is_model_registered? → resolve(undefined)
        │
        └─ exec_register_model
               ▼
          bootstrap                       (bootstrap.rs:78)
          [pool.get() → lock_client]
          [acquire_advisory_lock(zs_reg:<app>, register_model)]
          ┌── inner: async {
          │     ensure_app_schema
          │     ensure_audit_table
          │     next_schema_version
          │     expand declared_indexes
          │   } ──Err──► explicit pg_advisory_unlock + drop(lock_client)   ← R1/M3 FIXED (3bb41fa1)
          └── Ok → (RegisterContext, lock_client)
               ▼
          compute_plan                    (plan.rs:34)
          [introspect_schema + estimate_row_count]
          [compute_diff → Vec<DiffOp>]
               ▼
          validate                        (validate.rs:53)
          [destructive + strictness != off → write Pending audit rows]
          [strict   → Err(envelope_json)]                              ← lock released by mod.rs:213-227
          [lenient  → Ok(ApprovedPlan{ ops: plan.ops })]                ← Pending rows ORPHAN (F2)
          [off      → Ok(ApprovedPlan{ ops: plan.ops })]
               ▼
          apply                           (apply.rs:37)
          ┌─ Pass 1 (under advisory lock) ────────────────────────────┐
          │  for op in approved.ops:                                   │
          │    if op.class == Destructive → continue                   │
          │    if AddIndex → continue                                  │
          │    check_destructive_invariant(op)?  ← R1/C2 FIXED (3ef6a170)
          │    write_audit_row(status=Running)                         │
          │    pool_exec(op.sql)                                       │
          │    update_audit_status(Applied|Failed)  ← errors discarded (F1)
          └────────────────────────────────────────────────────────────┘
          explicit pg_advisory_unlock + drop(lock_client)
          propagate Pass-1 error
          ┌─ Pass 2 (unlocked) ────────────────────────────────────────┐
          │  for op in approved.ops (AddIndex only):                   │
          │    create_index_with_recovery (CIC + audited retry loop)   │
          └────────────────────────────────────────────────────────────┘

Backfill orchestrator (migrations.rs):

  exec_begin   ── acquire_dedicated_client + try_advisory_lock(zs_mig:<app>, name)
                  insert OR set_backfill_running, capture start_generation
                  park client in ctx.mig_lock
       ▼
  exec_fetch_batch  ── peek status; SELECT … WHERE id > cursor LIMIT n; heartbeat
       ▼
  exec_commit_batch ── BEGIN
                       lock_audit_row_for_update         ← serialises vs cancel/reset
                       if status='cancelled'     → ROLLBACK, err_cancelled_mid_run
                       if generation drift       → ROLLBACK, err_reset_externally
                       apply per-row UPDATEs
                       COMMIT/ROLLBACK                   ← row-lock released here
                       ── update_backfill_progress       ← runs UNLOCKED (F3 race window)
                       if is_done → finalise + release_advisory_lock + clear_mig_lock
```

### Audit-row state machines

**DDL** (validate + apply):
```
                                ┌─── Pending (validate-refused destructive)  ← terminal in practice (orphan)
                                │
write_audit_row ─────► Running ─┼── update_audit_status ──► Applied
                                │                       └─► Failed
                                │
                                └── (worker dies / update error) ──► Running FOREVER (F1)
```

**Backfill** (migrations):
```
                                  ┌── progress ──► Running (cursor/dead-letter/heartbeat)
                                  ├── operator reset ──► Pending (audit_generation += 1)
INSERT phase='backfill' ──► Running ─┤── operator cancel ──► Cancelled
                                  └── commit_batch(isDone) ──► Applied / AppliedWithDeadLetter / Failed
```

---

## 2. Status of R1 findings

| R1 ID | Status at HEAD | Fix commit |
|---|---|---|
| C1 — replication schema/publication case mismatch | **FIXED** | `a00c41fd` — `crate::query::quote_ident(app_id)` used for the SCHEMA reference, slot/pub names continue to use `sanitise_app_id` |
| C2 — `DropColumn`/`DropIndex` silent no-op | **FIXED** | `3ef6a170` — `check_destructive_invariant(op)?` at `apply.rs:62`; the `match` arm at `apply.rs:155-157` now returns `destructive_invariant_error(op)`; unit tests at `apply.rs:329-…` |
| I1 — split lenient invariant (validate keeps destructive ops, apply skips) | **OPEN** | No structural barrier. `apply.rs:203, 237` is the only gate. |
| I2 — cursor advance not atomic with COMMIT | **OPEN, now elevated to F3** | `migrations.rs:577-598` still puts `update_backfill_progress` after COMMIT. |
| I3 — resume from `running` row without prior-owner check | **OPEN, accepted** | Behavioural by design (`migrations.rs:284-297`). |
| I4 — orphan `Pending` audit rows for lenient destructive | **OPEN, now F2** | `validate.rs:74-85` writes them; no sweeper exists. |
| I5 — `pg_index` validity check string-interpolates SQL escape | **OPEN** | `backend/postgres.rs:461-464` still `format!`s `replace('\'', "''")`. |
| M1 — `validate_cursor` naming | **OPEN, naming debt** | Same column name in use. |
| M2 — DROP-slot LATERAL-CTE comment misleading | **OPEN, doc-only** | `replication.rs:436-438` matches the code now ("we SELECT first, then DROP per-row …"). |
| M3 — bootstrap lock leak on `ensure_app_schema`/`ensure_audit_table` error | **FIXED** | `3bb41fa1` — `bootstrap.rs:140-187` captures into `inner` then explicit-unlocks + drops on Err. |

Three CRITICAL/IMPORTANT items from R1 (C1, C2, M3) are fixed. The remaining issues, plus three new findings (F1/F2/F3), are below.

---

## 3. New findings (R2)

### CRITICAL

**(none — three R1 CRITICAL/IMPORTANT items resolved at this revision; no new CRITICAL surfaced in re-audit)**

### IMPORTANT

**F1 — Apply layer can permanently orphan `Running` DDL audit rows. `update_audit_status` errors are silently discarded; no sweeper.**

- **Why:** `apply.rs:163-186` ends both Ok and Err branches with `let _ = backend.update_audit_status(...).await;`. If the terminal-transition UPDATE fails (transient pool error, connection drop, statement timeout) the audit row stays `Running` forever. The DDL itself may have succeeded — the row is now a *false negative* for the deploy pipeline's "what's still in flight?" query.
- **Compounding factor:** the audit-row INSERT at `apply.rs:64-87` goes through the **pool** (not `lock_client`). It commits immediately and survives both a Pass-1 error and a `pg_advisory_unlock` failure. There is no link between the audit row and the `lock_client`'s session, so the operator cannot reap orphan `Running` rows by detecting a dead session (the way `migrations.rs` uses `owner_session_id` + `last_heartbeat_at` for backfill rows).
- **Crash impact:** if the worker dies between `write_audit_row` (Running) and `update_audit_status` (terminal), the row is permanently `Running`. The next deploy's audit-table scan sees a stale "in-flight" DDL it can't reconcile.
- **Fix:**
  1. Add `owner_session_id` + `last_heartbeat_at` to the DDL audit-write site so a watchdog can reap `Running` DDL rows whose owning session is dead.
  2. Retry `update_audit_status` once with a short backoff before the `let _` — at minimum a `tracing::warn!` so the silent discard is visible.
  3. Long-term: ship a sweeper analogous to `replication::drop_abandoned_slots` that scans `__zeroship_migrations` for `phase='ddl' AND status='running'` with stale heartbeat, downgrades them to `failed` with a synthesised error.
- **Verification:** `apply.rs:163-188` (the `let _ =` discard); `audit.rs:338-368` (`update_audit_status` returns `Ok(true|false)` — only the migration-orchestrator path consumes the bool, the apply path ignores it).

**F2 — `Pending` audit rows written by `validate` for refused destructive ops are never transitioned to a terminal state.**

- **Why:** `validate.rs:67-85` writes a `Pending` row per destructive op when `strictness != "off"`. For `strict` the function then returns `Err(envelope_json)` and the pipeline aborts — the Pending row persists. For `lenient` validate returns Ok with destructive ops still in `plan.ops`; the apply loop's destructive skip (`apply.rs:203, 237`) means `run_op` is never invoked for them, so `update_audit_status` is never called. **Both modes orphan the Pending row.**
- **Operator impact:** queries like `SELECT * FROM "<app>"."__zeroship_migrations" WHERE status='pending'` will accumulate phantom rows forever — one per refused destructive op per deploy. A manual-approve / drift-detection UX is impossible without distinguishing "actionable pending" from "historical refusal".
- **Fix:** either (a) transition the Pending row to a new terminal status — `validation_refused` already exists in the proposal A3 vocabulary; the `CHECK (status IN …)` constraint at `audit.rs:230` currently does NOT permit it, so the constraint and the `TerminalStatus` enum would both grow that variant; or (b) write the row with `status='running'` and immediately follow with `update_audit_status(Failed, "validation refused")` to mirror the apply-layer state machine.
- **Verification:** `validate.rs:74-85` (Pending write site); `validate.rs:88-93` (strict returns Err, lenient falls through); `apply.rs:203, 237` (destructive skip, so no terminal transition); `audit.rs:229-231` (`status` CHECK doesn't include `validation_refused`).

**F3 — `update_backfill_progress` runs OUTSIDE the BEGIN/COMMIT and without an `audit_generation` guard, creating a narrow window where an operator `reset` is silently lost.**

- **Why:** `migrations.rs:577` issues COMMIT — this releases the row lock acquired at `lock_audit_row_for_update` (line 487). `migrations.rs:585-598` then calls `update_backfill_progress` *without* re-checking `audit_generation`. The UPDATE statement at `audit.rs:725-754` writes `validate_cursor = $2`, `dead_letter_pks = $3`, `details.processed = $4` keyed only by `id` — no generation predicate.
- **Race:** between COMMIT (line 577) and `update_backfill_progress` (line 585) an operator who calls `migrations.reset(...)` runs `reset_backfill_row_pool` (audit.rs:627-649) which sets `status='pending'`, `validate_cursor=NULL`, bumps `audit_generation`. The subsequent `update_backfill_progress` then writes `validate_cursor = next_cursor` — clobbering the reset. The reset is silently lost; a fresh exec_begin reads the stale cursor and resumes from there instead of zero.
- **Why R1 deferred:** R1 framed this as "non-exactly-once after a crash" (cursor not advanced after COMMIT). The crash framing is the documented contract (`migrateOne` is the user's responsibility to make idempotent). The **reset clobber** is the corruption case the same gap also opens, and is NOT user-responsibility — it's a pipeline invariant break.
- **Fix:** include the progress UPDATE inside the BEGIN/COMMIT (move it BEFORE the COMMIT statement), OR add `AND audit_generation = $5::bigint` to the WHERE clause of `update_backfill_progress` so a reset breaks the update silently — the SDK then detects the no-op via affected-row-count and surfaces `migration_reset_externally` on the next batch.
- **Verification:** `migrations.rs:575-598` (COMMIT then update_backfill_progress); `audit.rs:725-754` (UPDATE without generation predicate); `audit.rs:627-649` (reset bumps generation, clears cursor); `migrations.rs:497-510` (the existing Gap-X guard runs only INSIDE BEGIN/COMMIT, not for the post-commit progress write).

### MINOR

**F4 — `validate.rs:88-93` lenient mode: split invariant remains (R1/I1).**

The structural fix (a `LenientApprovedPlan` type that has destructive ops stripped at the boundary) is still unimplemented. `apply.rs:203, 237` is the only gate. The `check_destructive_invariant` contract added at `apply.rs:62` is downstream of that gate — it traps a misclassified op *if it reaches `run_op`*, but a destructive op stripped of its class would still slip through the upstream skip. Net: defence-in-depth is one layer; making it two would require splitting the ApprovedPlan type by mode.

- File: `crates/plugin-db/src/orchestrator/register_model/validate.rs:88-106`, `apply.rs:203, 237`

**F5 — No manual-approve / drift-detection path for destructive migrations.**

The lens prompt asked: "what handles destructive migrations explicitly? Is there a manual-approve path? Drift detection?"

Answer:
- **strict** (default) — refused at validate. Operator response: redesign the schema OR override `strictness` for one deploy.
- **lenient** — silently skipped at apply (with orphan Pending row, see F2).
- **off** — applied unconditionally.

There is no `migrations.approve(<audit_id>)` SDK call, no operator UI hook in this crate, no "drift" predicate that compares live schema to declared and flags unaccounted columns. Operators relying on `WHERE status='pending'` to drive an approval workflow would also pick up F2's orphan rows.

This is **scoped out by proposal A2** in the present revision — surfaced for awareness. Adding the approve path would require: (a) F2 fix (clean terminal state for refusals), (b) a new SDK surface, (c) operator authentication at the boundary (the `actor` enum currently only has `Auto`).

- File: `crates/plugin-db/src/orchestrator/register_model/validate.rs` (no approve path); `crates/plugin-db/src/audit.rs:78-90` (`ActorKind` has only `Auto`).

**F6 — `create_index_with_recovery_audited`: retry budget exhaustion code path returns `cic_configuration` instead of `validation_refused`.**

`backend/postgres.rs:590-602` — the loop exits without a terminal return. This is supposed to be unreachable: every iteration writes a terminal `return`. If a future refactor adds a new branch that drops out of the loop without a return, the operator sees `cic_configuration` instead of a proper retry-exhausted envelope. The current code is correct (the loop always returns from inside); the exit fallback is a defence-in-depth tripwire. The choice of `Configuration` (vs `SchemaRefused { code: "cic_failed" }` like every other terminal arm) makes the failure mode look like a config issue rather than a retry-exhaustion in operator dashboards. Cosmetic but real.

- File: `crates/plugin-db/src/backend/postgres.rs:587-602`.

**F7 — Concurrent `register_model` from different deploys is correctly serialised by the per-app advisory lock; cross-deploy ordering is "whoever blocks first wins."**

The lock at `bootstrap.rs:103-119` uses `hashtext('zs_reg:<app_id>')` + `hashtext('register_model')`. Two simultaneous deploys for the same app serialise. Cross-app deploys don't serialise (different key). `deploy_id` is read from `ZEROSHIP_DEPLOY_ID` at `mod.rs:130`. The advisory lock fix from `3bb41fa1` (bootstrap), `b4e533e2` (plan/validate), and `37a0ef76` (apply Pass 1) means the lock is now released on every error path. No outstanding leak class in the four-stage pipeline.

The only remaining lock-leak window is **future cancellation** of the `spawned_ops` future at `mod.rs:85` while `run_pipeline` is mid-flight (e.g. isolate teardown). On Drop, `lock_client` (PooledClient) goes straight back to the pool without an unlock (see `compio-postgres/src/pool.rs:812-818` — the Drop impl doesn't release session locks). Worker shutdown therefore strands the advisory lock until the connection task ends. In practice the connection task is cancelled by the same shutdown, so the session ends and the lock auto-releases — but the ordering is unsupervised. Not actionable; noted for completeness.

- File: `crates/plugin-db/src/orchestrator/register_model/mod.rs:85-103`; `crates/compio-postgres/src/pool.rs:812-818`.

**F8 — `migration` advisory lock cannot be reclaimed without isolate teardown when a Migration wrapper is GCed.**

`v8_classes/migration.rs:94-131` — the `Drop` impl spawns `exec_cancel` which flips the audit row to `cancelled` but does NOT call `release_active_lock` or clear `ctx.mig_lock`. The lock client therefore stays parked in the slot; the advisory lock is held until the isolate dies. The next `exec_begin` on the same isolate hits `migration_already_active` at `migrations.rs:236-240` and the isolate is wedged for that migration name.

The module comment at `v8_classes/migration.rs:106-108` acknowledges this ("The advisory lock will still be released when the owning thread's migration lock client is dropped on isolate teardown.") This is documented behaviour, not a bug per se, but it means a single mis-cancelled run can wedge the isolate for an arbitrary duration.

- File: `crates/plugin-db/src/v8_classes/migration.rs:84-131`; `crates/plugin-db/src/migrations.rs:233-240` (the wedge predicate).

**F9 — `pg_index` validity check string-interpolates SQL escape (R1/I5, unchanged).**

Same site as R1, unchanged at `backend/postgres.rs:461-464`. With `standard_conforming_strings = on` (the default on all supported PG versions) the `replace('\'', "''")` is correct. With it OFF, a `\` in the index name could cap-shift the escape. Index names come from `query::build_named_indexes` / `query::build_create_indexes`, both of which restrict the character set, so this is a latent issue not a current bug.

---

## 4. Stage invariants (HEAD)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Lock held on `lock_client`; schema + audit table exist; schema_version monotonic; indexes expanded | Err → explicit unlock + drop (3bb41fa1) |
| **plan** | Lock held; audit table exists | `Vec<DiffOp>` classified | Err → caller (mod.rs:213-227) unlocks + drops |
| **validate** | Lock held | `ApprovedPlan` with destructive ops retained (lenient/off) or short-circuit (strict). **Pending audit rows for refused destructive ops are NOT terminalised** (F2) | strict Err → caller unlocks + drops |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows written; lock released unconditionally before pass 2; `check_destructive_invariant` traps misclassified Drops (3ef6a170) | Err → unlock + drop, then propagate (apply.rs:215-233) |
| **apply pass 2** | Lock released | CIC ops run idempotently; retry loop with `pg_index` indisvalid check + audited retry log | terminal → `SchemaRefused { code: "cic_failed" }` or `cic_configuration` (F6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held; BEGIN open | Audit row locked FOR UPDATE; generation check inside tx; per-row UPDATEs applied; COMMIT/ROLLBACK. **`update_backfill_progress` runs UNLOCKED** (F3) | dry-run → ROLLBACK; real run → COMMIT + progress write; is_done → finalise + release advisory lock |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. Does NOT release the in-flight worker's advisory lock (which serialises via FOR UPDATE) | benign |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, **audit_generation += 1** | Gap-X guards in-flight commits — except for the F3 window |

---

## 5. Score

**81 / 100** (vs **72 / 100** in R1)

### Movement
- **+9 net** from three resolved findings (R1/C1, R1/C2, R1/M3) — each was a real corruption / stall vector.
- Three new findings (F1, F2, F3) are IMPORTANT; F1 and F3 are the most actionable. F2 is high-noise but low-corruption.
- Six unchanged findings (R1/I1 → F4, R1/I2 → F3 elevated, R1/I3, R1/I4 → F2 elevated, R1/I5 → F9, R1/M1, R1/M2).

### Remaining deductions
- **F1 (-6):** orphan `Running` DDL audit rows are a pipeline-state corruption observable by operators every time a worker dies mid-DDL.
- **F2 (-4):** orphan `Pending` audit rows pollute the operator surface for every lenient destructive deploy.
- **F3 (-5):** narrow but real reset-clobber race; the existing Gap-X protections terminate one step too early.
- **F4-F9 (-4 combined):** minor / accepted.

### Comparison to R1 score breakdown
R1 deducted heavily for C1 (silent WAL fan-out for mixed-case apps), C2 (silent DDL audit lies), and M3 (cross-app stall from bootstrap leak). All three are gone at HEAD. The R2 deductions are narrower in blast radius (audit-state hygiene vs. data-state corruption / cross-tenant stall) but the **count of pipeline-state corruption paths is roughly stable** — F1 + F3 + F2 replace the three CRITICAL-tier R1 items with three IMPORTANT-tier R2 items.

The pipeline is now substantially safer to run **per deploy**; the remaining defects accumulate over **operational time** (orphan audit rows, reset races against long-running migrations). The trajectory is positive.
