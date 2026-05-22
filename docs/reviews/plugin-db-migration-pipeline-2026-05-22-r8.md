# plugin-db: Migration / DDL Pipeline Correctness Review (R8)

**HEAD:** `9e392ba1` (post-r7 cycle 07:30 → 09:00) · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85/100)

---

## 0. Scope of this round

Per the dispatch brief, the plugin-db delta since r7 is **one** commit:

| Commit | Module | Migration-pipeline relevance |
|---|---|---|
| `9e392ba1` | `error.rs` (top-of-file rustdoc preamble) | Pure docstring rewrite — accurate enumeration of `Result<_, String>` hold-outs (code-critique r8 R8-1). **Zero code-path change.** |

Verified via `git show --stat 9e392ba1`: `crates/plugin-db/src/error.rs | 43 +++++++++++++++++++++++++++++++------------ 1 file changed, 31 insertions(+), 12 deletions(-)`. The diff body is exclusively the module-level rustdoc block (lines 7-44, prefixed `//!`). No emitted code changes; the `prefix_message`, `coded_db`, `coded_sql`, and `DbError`-variant definitions are byte-for-byte identical to HEAD-at-r7.

The remaining 19 plugin-db commits in the same intra-day window
(`3ed6d456` through `9caf4f2e`) are all in `replication.rs`,
`replication_ops.rs`, `wal_consumer.rs`, `auth/session.rs`, and
docs/reviews — **none** intersect the migration-pipeline call graph
(verified again by Grep of pipeline filenames into those commits'
filelists; see §1.3).

Net structural change to the migration pipeline this round: **zero**.

Re-audit dimensions per the brief: **F1**, **F2**, **F3 / [I41]**,
**R5-M7**, **R5-M8** (cursor monotonicity), **CIC retry budget**,
**concurrent register_model + migration**, **plateau check**.

---

## 1. Audit dimensions — re-verified at HEAD

### 1.1 F1 (orphan Running DDL audit rows) — OPEN, 7+ cycle carry

**Status:** unchanged since r2. Five sites still discard the typed
`Result` of a terminal-transition write via `let _ =`:

| # | File:line | Site |
|---|---|---|
| 1 | `orchestrator/register_model/apply.rs:163-170` | DDL Applied transition |
| 2 | `orchestrator/register_model/apply.rs:178-186` | DDL Failed transition |
| 3 | `backend/postgres.rs:478-486` | CIC `invalid_index_landed` Failed |
| 4 | `backend/postgres.rs:519-528` | CIC `data_violation` Failed |
| 5 | `backend/postgres.rs:558-567` | CIC `transient_retry` / `non_transient_failure` Failed |

Companion gap: the DDL `write_audit_row` INSERT at `audit.rs:286-308`
still omits `owner_session_id` / `last_heartbeat_at` from its column
list:

```sql
INSERT INTO "{app_id}"."__zeroship_migrations"
    (collection, phase, change_class, change_kind, details,
     ddl_sql, status, deploy_id, applied_by_kind, schema_version)
    VALUES ($1, $2, $3, $4, $5::jsonb, $6, $7, $8, $9, $10::integer)
    RETURNING id
```

The backfill INSERT (`audit.rs:561-569`) and UPDATE
(`audit.rs:530-538`) DO stamp `pg_backend_pid()::text` +
`NOW()` into those columns. DDL/backfill column-coverage asymmetry
makes a heartbeat-driven sweeper impossible to write without first
filling the columns on the DDL rail. **Unchanged from r7.**

### 1.2 F2 (orphan validate-refused Pending) — OPEN, 7+ cycle carry

**Status:** unchanged. `validate.rs:69-87` still writes
`InitialStatus::Pending` per destructive op. Two terminal paths:

- **strict** (`validate.rs:91`): `return Err(envelope)`, the Pending
  row is never driven to a terminal status.
- **lenient** (`validate.rs:93-94` fall-through): destructive ops
  retained in `plan.ops`, `apply.rs:203 / 233` skips them, the
  Pending row is never reached again.

Status CHECK at `audit.rs:218-220` still:

```sql
status IN ('pending','running','applied','applied_with_dead_letter',
           'failed','cancelled','rolled_back')
```

— missing `validation_refused`. Even if validate.rs wanted to flip
the row to a terminal "refused" state, the CHECK would reject SQLSTATE
23514. **Unchanged from r7.**

### 1.3 F3 / [I41] — CLOSED, re-verified

`exec_commit_batch` (`migrations.rs:436-660`) walked end-to-end against
HEAD:

```
line 462  client = take_lock_client()
line 473  client_exec("BEGIN")
line 487  lock_audit_row_for_update                 ← FOR UPDATE row lock
line 498  if row.status == "cancelled" → rollback_and_return; err_cancelled_mid_run
line 506  if row.audit_generation != start_generation → rollback_and_return; err_reset_externally
line 513  for upd in updates_arr: build UPDATE / client_exec
line 585  if !dry_run:
line 586    update_backfill_progress             ← inside tx, under row lock
line 606  final_sql = "COMMIT" | "ROLLBACK"
line 607  client_exec(final_sql)
line 614  if is_done:
line 635    finalise_backfill (tracing::warn! on Err)   ← R5-M7 close intact
line 650-651 release_advisory_lock + drop(client)
line 654  clear_mig_lock
```

`lock_audit_row_for_update` SQL (`audit.rs:686-688`) unchanged:
`SELECT status, audit_generation FROM "{app_id}"."__zeroship_migrations" WHERE id = $1::bigint FOR UPDATE`.

Row lock survives until COMMIT/ROLLBACK; cursor write inside the tx;
generation drift caught before the per-row UPDATEs. **[I41] still
closed.**

### 1.4 R5-M7 (finalise_backfill warn-on-err) — CLOSED, re-verified

```rust
// migrations.rs:635-648
if let Err(e) = backend
    .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
    .await
{
    tracing::warn!(
        app_id = %app_id,
        audit_id = audit_id,
        terminal = ?terminal,
        error = %e,
        "finalise_backfill failed; audit row may stay in 'running' \
         status until next reset() — investigate if the operator \
         sees stuck migrations"
    );
}
```

Byte-identical to r7. Lock release order (line 650-651: release ⇒
drop ⇒ clear_mig_lock) unchanged. **Closed; re-verified.**

### 1.5 R5-M8 / R6-M2 — cursor monotonicity SDK-trusted

`audit.rs:715-723` (`update_backfill_progress`):

```sql
UPDATE "{app_id}"."__zeroship_migrations"
   SET validate_cursor   = $2::bigint,
       dead_letter_pks   = $3::jsonb,
       details           = jsonb_set(COALESCE(details, '{}'::jsonb),
                                     '{processed}', to_jsonb($4::bigint)),
       last_heartbeat_at = NOW(),
       updated_at        = NOW()
 WHERE id = $1::bigint
```

No `validate_cursor < $2::bigint` predicate; SDK trusted to advance
monotonically. Per-row B1 idempotence makes rewind a
reprocess-not-a-skip; no data loss. **Latent; unchanged.**

### 1.6 CIC retry budget — re-walked

`create_index_with_recovery_audited` (`postgres.rs:385-601`).
`MAX_RETRIES = 3` → `for attempt in 0..=MAX_RETRIES` (4 iterations).

| Attempt outcome | Branch | Terminal? |
|---|---|---|
| `pg_index.indisvalid = true` | `Ok` (line 458-473) | ✓ `Ok(())` |
| `pg_index.indisvalid = false` | `Ok` (lines 475-499) | loop unless `attempt == MAX_RETRIES` → `cic_failed` |
| SQLSTATE in {23505,23502,23503,23514} | `Err`-fatal (501-539) | ✓ `unique_violation` envelope |
| SQLSTATE in {40P01,53100,53200} | `Err`-transient (541-583) | loop unless `attempt == MAX_RETRIES` → `cic_failed` |
| Other SQLSTATE | `Err`-other (548-583) | ✓ `cic_failed` (no retry) |
| Loop exits without terminal return | line 593-600 | `cic_configuration` (defence-in-depth tripwire) |

Per attempt, partial-index cleanup always runs (lines 487 / 529 /
569: `DROP INDEX CONCURRENTLY IF EXISTS`); a Running audit row and a
best-effort Failed terminal write fire on every retry (these are 3 of
the 5 F1-family sites).

`MAX_RETRIES = 3` is unchanged; classification table unchanged;
fallback at 593-600 unchanged. **Sound; unchanged from r7.**

### 1.7 Concurrent `register_model` + `migrations.begin` — disjoint key spaces

| Caller | Namespace | Tag | Mode |
|---|---|---|---|
| `register_model` | `hashtext("zs_reg:<app>")::int4` | `hashtext("register_model")::int4` | blocking `pg_advisory_lock` |
| `migrations.exec_begin` | `hashtext("zs_mig:<app>")::int4` | `hashtext(<name>)::int4` | non-blocking `pg_try_advisory_lock` |

Evidence:

- `orchestrator/register_model/bootstrap.rs:59-63` —
  `pub(crate) fn lock_key(app_id: &str) -> String { format!("zs_reg:{app_id}") }`
  + `pub(crate) const LOCK_TAG: &str = "register_model";`
- `migrations.rs:260` — `let lock_key = format!("zs_mig:{app_id}");`
  + `name` as the tag at line 262.

Joint hashtext-space collision: ~1/2⁶⁴ per (app, name) pair. On
collision, the failure mode is: `register_model` queues (blocking
acquire); `migrations.exec_begin` receives `false` from
`pg_try_advisory_lock` and returns `err_already_running()` — coded
SDK error, no hang. **Effectively zero risk; unchanged.**

### 1.8 deeefe18 — wire-shape preservation re-verified

The r7 deeefe18 audit walked all 18 `coded_db` call sites and the
shared `prefix_message` body. At HEAD-r8:

- `migrations.rs:84-93` `fn coded_db` body is unchanged (delegates to
  `crate::error::prefix_message`).
- `error.rs:370-388` `pub(crate) fn prefix_message` body is unchanged
  (same 8 prefix-eligible variants, same `_ => {}` arm).
- `error.rs:721 / 786` — `prefix_message_preserves_variant_and_code`
  and `prefix_message_leaves_structured_variants_alone` tests still
  present (only line numbers shifted by ~7 due to the docstring
  insert at the top of file).

Wire shape contract holds at all 18 sites. **Closed; re-verified.**

### 1.9 `__zeroship_migrations` table schema — no changes

`ensure_audit_table_exists` (`audit.rs:181-261`) is byte-identical to
r7. 23 columns; `audit_generation BIGINT NOT NULL DEFAULT 0` added
idempotently via `ALTER … ADD COLUMN IF NOT EXISTS`. Status CHECK
still lacks `validation_refused`. Phase CHECK
(`'ddl','validation','backfill','audit'`) unchanged.

---

## 2. Pipeline diagram at HEAD (r8)

Functionally identical to r7. The only delta is a top-of-file
docstring in `error.rs`. Reproduced for cross-round reference:

```
register_model_dispatch                  (orchestrator/register_model/mod.rs)
        │
        ├─ [fast path] is_model_registered? → resolve(undefined)
        │
        └─ exec_register_model
               ▼
          bootstrap.rs
            pool.get() → lock_client
            OrchestratorLockGuard::acquire           (#[must_use] 808a32af)
            ├── build_ctx: ensure_app_schema /
            │   ensure_audit_table /
            │   next_schema_version /
            │   expand declared_indexes
            │   ──Err──► guard.release().await; return Err(e)
            └── Ok → (RegisterContext, OrchestratorLockGuard<'p>)
               ▼
          compute_plan         ──Err──► mod.rs guard.release().await
               ▼
          validate
            destructive + strictness != "off" → Pending audit rows (F2 ORPHAN)
            strict   → Err(envelope) → wrap as DbError::SchemaRefused
            lenient  → Ok(ApprovedPlan{ destructive RETAINED }) (F2 ORPHAN)
            off      → Ok(ApprovedPlan{ destructive RETAINED })
               ▼
          apply
          ┌─ Pass 1 (under advisory lock — apply.rs:201-213) ───────────┐
          │  for op in approved.ops:                                     │
          │    skip Destructive; skip AddIndex                           │
          │    check_destructive_invariant(op)?                          │
          │    write_audit_row(Running)                                  │
          │    pool_exec(op.sql) | create_index_with_recovery (AddIndex) │
          │    update_audit_status(Applied|Failed)  ← `let _ =`  (F1)    │
          └──────────────────────────────────────────────────────────────┘
          guard.release().await           (apply.rs:226 — always)
          pass1?                          (propagate after release)
          ┌─ Pass 2 (unlocked) ──────────────────────────────────────────┐
          │  for op in approved.ops (AddIndex only): run_op              │
          └──────────────────────────────────────────────────────────────┘

Backfill orchestrator (migrations.rs):
  exec_begin          → acquire_dedicated_client + try_advisory_lock(zs_mig:<app>, name)
                        ensure_audit_table (map_audit_bootstrap_err)
                        find_latest_backfill_row → snapshot start_generation
                        insert_backfill_running OR set_backfill_running ← no gen-check (R8-M1)
                        park client in ctx.mig_lock
       ▼
  exec_fetch_batch    → peek_latest_backfill_status; SELECT WHERE id > cursor LIMIT n;
                        heartbeat_backfill (best-effort)
       ▼
  exec_commit_batch   → BEGIN
                        lock_audit_row_for_update          ← FOR UPDATE row lock
                        if status='cancelled'     → rollback_and_return; err_cancelled_mid_run
                        if audit_generation drift → rollback_and_return; err_reset_externally
                        for upd: UPDATE schema.table SET … WHERE id = $1
                        update_backfill_progress           ← R4 [I41] FIX preserved
                        COMMIT/ROLLBACK
                        if is_done → finalise_backfill (`if let Err = ... tracing::warn!`)  ← R5-M7 CLOSED
                                   + release_advisory_lock + drop(client) + clear_mig_lock
```

---

## 3. R8 findings

### CRITICAL

(none)

### IMPORTANT

**[R8-I1] crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186 — F1: Orphan `Running` DDL audit rows (carry-forward from R7-I1 / R6-I1 / R5-I1 / r2).**

  Why: data loss / stall / cross-app impact:
    No data loss; no stall (the OrchestratorLockGuard always releases
    on observable Err paths). Observability rot — per-deploy at least
    one stale `Running` row possible after a transient terminal-
    UPDATE failure, indistinguishable from a real in-flight DDL by an
    operator scan. The DDL `write_audit_row` INSERT
    (`audit.rs:286-308`) further omits `owner_session_id` /
    `last_heartbeat_at`, so a future heartbeat-driven sweeper has no
    column to key on for the DDL rail. Blocks any future drift-
    detection / manual-approve UX.

  Fix:
    1. Add `owner_session_id = pg_backend_pid()::text,
       last_heartbeat_at = NOW()` to the DDL INSERT column list
       (`audit.rs:286-308`).
    2. Replace each `let _ = update_audit_status(...).await;` with
       `if let Err(e) = ... { tracing::warn!(audit_id = id, error = %e,
       "audit: terminal-transition update failed; row stays Running
       until sweeper reaps"); }`. Five sites: `apply.rs:163-170`,
       `apply.rs:178-186`, `postgres.rs:478-486`, `postgres.rs:519-528`,
       `postgres.rs:558-567`. The `migrations.rs:635-648` close is
       the in-tree model.
    3. Ship a heartbeat-driven sweeper: scan
       `phase='ddl' AND status='running' AND (last_heartbeat_at IS
       NULL OR last_heartbeat_at < NOW() - INTERVAL '5 min')` and
       demote to `failed`. Separate work item; ~30-50 LOC.

  Verification:
    `apply.rs:163-170` (Applied `let _ =`); `apply.rs:178-186`
    (Failed `let _ =`); `audit.rs:286-308` (DDL INSERT — column list
    `(collection, phase, change_class, change_kind, details, ddl_sql,
    status, deploy_id, applied_by_kind, schema_version)` — no
    session/heartbeat columns); `audit.rs:530-538`
    (`set_backfill_running` DOES stamp `pg_backend_pid()::text +
    NOW()` — asymmetry); `audit.rs:561-569`
    (`insert_backfill_running` DOES stamp pid+heartbeat — asymmetry).

**[R8-I2] crates/plugin-db/src/orchestrator/register_model/validate.rs:66-95 — F2: Orphan `Pending` validate-refused audit rows (carry-forward from R7-I2 / R6-I2 / R5-I2 / r2).**

  Why: data loss / stall / cross-app impact:
    No data loss; no stall. Observability pollution — the audit
    table grows unboundedly with phantom Pending rows on every strict
    refusal and every lenient destructive-skip. The status CHECK
    constraint (`audit.rs:218-220`) does not include
    `validation_refused`, so even if `validate.rs` wanted to emit a
    terminal row it would hit SQLSTATE 23514.

  Fix:
    1. Extend `__zeroship_migrations_status_chk` (`audit.rs:218-220`)
       to include `validation_refused`. Use `ALTER TABLE … DROP
       CONSTRAINT … / ADD CONSTRAINT …` inside `ensure_audit_table`
       (idempotent via `DO $$ … $$`).
    2. Add `TerminalStatus::ValidationRefused` to
       `audit::TerminalStatus` with `as_sql() = "validation_refused"`.
    3. In `validate.rs:67-88` change the Pending-write to a
       Running-write + immediate
       `update_audit_status(ValidationRefused, ...)` — mirrors the
       apply-layer state-machine pattern; no new `InitialStatus`
       variant needed. ~15 LOC total.

  Verification:
    `validate.rs:69-87` (Pending write site,
    `InitialStatus::Pending`); `validate.rs:90-92` (strict returns
    Err with no terminal transition); `validate.rs:93-94` (lenient
    falls through with destructive ops retained); `apply.rs:203 +
    233` (destructive skip path — never reaches
    `update_audit_status`); `audit.rs:218-220` (status CHECK
    constraint — `validation_refused` not in IN-list).

### MINOR

**[R8-M1] crates/plugin-db/src/migrations.rs:293-296 — `set_backfill_running` lacks generation predicate (carry-forward R7-M1 / R6-M1 / R5-M1).**

  Why: data loss / stall / cross-app impact:
    No data loss. Transient UI ghost: an operator `migrations.reset`
    between the `find_latest_backfill_row` SELECT at
    `migrations.rs:279-282` and the `set_backfill_running` UPDATE at
    `migrations.rs:293-296` flips the row's `audit_generation` from
    `g0` to `g0+1`. `set_backfill_running` runs
    `UPDATE … SET status = 'running' … WHERE id = $1::bigint`
    (`audit.rs:530-538`) and overwrites the reset's `'pending'`. The
    reset isn't *lost* — the next `commit_batch`'s FOR-UPDATE +
    generation check (`migrations.rs:506-509`) catches it and surfaces
    `err_reset_externally` — but the row briefly shows
    `status='running'` to operator-side audit scans. Self-heals at
    next batch. Cosmetic.

  Fix:
    Add `AND audit_generation = $2::bigint` to
    `set_backfill_running`'s WHERE (`audit.rs:530-538`) and pass
    `row.audit_generation` from `migrations.rs:294`; on 0 rows
    affected, return `err_reset_externally()` directly so the SDK
    mints a fresh wrapper. ~5 LOC.

  Verification:
    `migrations.rs:279-296` (snapshot-then-set is non-transactional,
    no gen-check passed); `audit.rs:530-538` (UPDATE keyed only by
    id).

**[R8-M2] crates/plugin-db/src/audit.rs:707-736 — `update_backfill_progress` cursor monotonicity SDK-trusted (carry-forward R7-M2 / R6-M2 / R5-M8).**

  Why: data loss / stall / cross-app impact:
    Narrow. A buggy SDK or rolled-back-batch-then-retry path that
    re-issues `commit_batch` with a stale `nextCursor` would silently
    rewind the cursor. Risk-of-reprocessing, not risk-of-skip
    (per-row migrations are idempotent — B1 contract). No data loss.

  Fix:
    Add `AND (validate_cursor IS NULL OR validate_cursor < $2::bigint)`
    to the WHERE; on 0 rows affected, surface
    `migration_cursor_rewind` coded error so the SDK can re-snapshot.
    ~4 LOC.

  Verification:
    `audit.rs:715-723` (UPDATE keyed only by id; no monotonicity
    predicate).

**[R8-M3] crates/plugin-db/src/orchestrator/lock_guard.rs:212-240 — Drop-path panic still mitigated only by logs (carry-forward R7-M3 / R6-M3 / R5-M3).**

  Why: cross-app impact:
    A panic during pipeline execution → `Drop` logs `tracing::error!`
    "leak:" → pooled client returns to pool with session-scoped
    `pg_advisory_lock(zs_reg:<app>, register_model)` held → every
    subsequent `register_model` for that app stalls until the
    backend session closes (typically pool recycle, tens of seconds
    to minutes). Drop is sync; cannot run async
    `pg_advisory_unlock`.

  Fix:
    Wrap the inner `run_pipeline` body in
    `std::panic::AssertUnwindSafe(...).catch_unwind()` at the
    dispatch boundary; on panic, explicitly run the unlock SQL via a
    synchronous fast-path (or by reissuing a fresh `Client`) before
    re-raising. Documented but unshipped since r4.

  Verification:
    `lock_guard.rs:214-237` (Drop body: `tracing::error!` only;
    comment explicitly acknowledges the limitation at lines 215-222).

**[R8-M4] crates/plugin-db/src/backend/postgres.rs:459-462 — `pg_index` validity check uses string-interpolated `indexrelid` (carry-forward R7-M4 / R6-M4 / R5-M4).**

  Why:
    `SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass`
    interpolates `qualified_idx` via `replace('\'', '\'\'')`. Index
    names come from `query::build_*_indexes` (caller-controlled and
    upstream-validated); still belt-and-braces.

  Fix:
    `query_text_params("… WHERE indexrelid = $1::regclass",
    &[qualified_idx.as_str()])`. ~3 LOC.

  Verification:
    `postgres.rs:459-462`.

**[R8-M5] crates/plugin-db/src/v8_classes/migration.rs + crates/plugin-db/src/migrations.rs (exec_cancel) — operator path strands the in-flight worker's lock client (carry-forward R7-M5 / R6-M5 / R5-M5).**

  Why: stall:
    `exec_cancel` writes `status='cancelled'` to the audit row via
    the pool (no lock) and returns. The in-flight worker's parked
    `mig_lock` client is not touched. The worker's next `fetchBatch`
    reads the cancelled status and aborts; the lock releases when
    the worker calls `clear_mig_lock` or when the isolate tears
    down. The cancelled migration's `(name, collection)` is wedged
    on that isolate for up to one batch interval before the cancel
    observation triggers the abort + release. No cross-app impact
    (lock keyed on `(zs_mig:<app>, <name>)`).

  Severity:
    Not actively a worsening. Same posture as r7.

**[R8-M6] crates/plugin-db/src/backend/postgres.rs:593-600 — CIC retry-exhaustion fallback variant divergence (carry-forward R7-M6 / R6-M6 / R5-M6).**

  Why:
    Loop-exit-without-terminal returns
    `DbError::Configuration { code: "cic_configuration" }` while
    every other terminal arm returns
    `SchemaRefused { code: "cic_failed" }`. Cosmetic; defence-in-
    depth tripwire. Unreachable in current code.

  Fix:
    Harmonise the fallback code to `cic_failed` for SDK consistency,
    or keep the divergent code with a documented "invariant breach"
    tag (the current state). ~3 LOC.

---

## 4. Stage invariants at HEAD (r8 — identical to r7)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Guard held (`#[must_use]`); schema + audit table exist; `schema_version` monotonic | Err → `guard.release().await; return Err` |
| **plan** | Lock held | `Vec<DiffOp>` classified ([I28]); `compute_diff` pure | Err → caller releases |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows NOT terminalised** (R8-I2 / F2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows Running→Applied/Failed; **terminal UPDATE errors silently discarded** (R8-I1 / F1); `check_destructive_invariant` traps misclassified Drops | Err → release (apply.rs:226), then `pass1?` propagates |
| **apply pass 2** | Lock released | CIC ops run idempotently (`IF NOT EXISTS`); retry loop with `pg_index` indisvalid check + audited retry log (4 attempts; deterministic SQLSTATE classification) | terminal → `SchemaRefused { code: "cic_failed" }`; fallback → `cic_configuration` (R8-M6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`set_backfill_running` lacks gen predicate** (R8-M1); `insert_backfill_running` raises `DbError::Internal` on empty RETURNING via `first_row_or_internal` | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held | BEGIN; row FOR UPDATE inside tx; generation check inside tx; per-row UPDATEs; **`update_backfill_progress` inside tx under row lock** (R4 [I41] preserved); COMMIT/ROLLBACK; is_done → `finalise_backfill` (`tracing::warn!` on Err — R5-M7 closed) + release + drop | dry-run → ROLLBACK; partial-failure → `rollback_and_return` |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release in-flight worker's mig advisory lock** (R8-M5) | benign for audit row; isolate wedged until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — closed at r4 |
| **OrchestratorLockGuard::release** | Guard live | `pg_advisory_unlock` issued (best-effort, logs `tracing::warn!` on SQL error); released flag set AFTER unlock await; returns unlocked PooledClient | SQL errors logged not swallowed silently |
| **OrchestratorLockGuard::drop** (panic / forgotten release) | n/a | `tracing::error!` with "leak:" prefix + diagnostic checklist (808a32af); PooledClient returns with lock held; auto-release on backend session close | Documented catastrophic fallback (R8-M3) |

---

## 5. Audit-row terminal-transition reliability (r8 re-tally)

Sites that discard a typed `Result` from a state-transition write
(the F1 family) — **unchanged from r7: 5 sites**.

1. `apply.rs:163-170` — DDL Applied transition (F1, R8-I1).
2. `apply.rs:178-186` — DDL Failed transition (F1, R8-I1).
3. `postgres.rs:478-486` — CIC `invalid_index_landed` Failed (F1 family).
4. `postgres.rs:519-528` — CIC `data_violation` Failed (F1 family).
5. `postgres.rs:558-567` — CIC `transient_retry` / `non_transient_failure` Failed (F1 family).

The R5-M7-style fix (`if let Err(e) = ... { tracing::warn!(...); }`)
applies uniformly. `migrations.rs:635-648` is the in-tree model.

---

## 6. Concurrent register_model + concurrent migration — re-verified

Lock-key construction unchanged at HEAD:

- `bootstrap.rs:59-63` — `lock_key("<app>") = "zs_reg:<app>"`
  + `LOCK_TAG = "register_model"` (blocking acquire).
- `migrations.rs:260` — `lock_key = "zs_mig:<app>"` + `name` as tag
  (try-acquire).

Hashtext-space collision: ~1/2⁶⁴ per (app, migration_name) pair.
Even on collision, `register_model` blocks; migration sees
`pg_try_advisory_lock = false` → `err_already_running()` coded error.
No cross-discipline interference. **Unchanged from r7.**

---

## 7. Plateau check

The dispatch brief asks: at what score is further migration-pipeline
review counterproductive without F1+F2 work landing?

**Read of the round:** since r5 the migration-pipeline reviewer has
made one net point per round on dimensions adjacent to F1/F2 (R5-M7
close at r6 ≡ +1; error-helper consolidation at r7 ≡ +1; nothing
shipping at this lens this round). The two open IMPORTANTs are
fundamentally schema-deployment work:

- **F2 (R8-I2)** requires `ALTER TABLE … DROP CONSTRAINT / ADD
  CONSTRAINT` against running deployments (the audit table is a
  per-app schema object; the constraint extension must run from
  `ensure_audit_table` so cold-starts pick it up; existing apps need
  an idempotent migration of the constraint itself). It's small
  (~15 LOC) but it's blocking on **migration story for the migration
  table** — recursive constraint.
- **F1 (R8-I1)** has two halves: the `tracing::warn!` extension is
  trivial (~15 LOC at five sites); the deployable value is the
  sweeper which is ~30-50 LOC plus an operational rollout (cron task
  or worker-tick callback). The sweeper additionally depends on the
  DDL INSERT picking up `owner_session_id` / `last_heartbeat_at`,
  which is itself a schema-change (idempotent column-add) on the
  same table.

**Plateau threshold estimate (this lens):**

- Without F1+F2 work landing, the asymptote is **86-87**. There is
  one further +1 available from R8-M1 (5 LOC, fully local) and
  another +1 available from R8-M4 (3 LOC, parameter-binding swap),
  totalling a possible **87** in this lens before plateau.
- After F1 (audit-row state machine 68→78) and F2 (validate stage
  CHECK + ValidationRefused), the lens unlocks to **90-92**: the
  remaining gap concentrates in R8-M3 (catch_unwind) and R8-M2
  (cursor monotonicity), both more disruptive next-stratum work.
- Beyond ~92, further migration-pipeline review at this lens needs
  to widen scope to include the **cross-isolate / cross-pod**
  invariants (e.g. how does the sweeper know which DDLs to demote
  when multiple workers concurrently mark Running?) which isn't
  pure-code work — it's distributed-systems design.

**Recommendation:** the lens has narrowed to two findings that need
landing. The marginal value of an r9 in this lens without those
shipping is approximately **+0**. Suggest pivoting to:

1. Land R8-I2 (F2 close) — biggest single jump.
2. Land R8-M1 — trivial.
3. Land R8-I1 *partial* (`tracing::warn!` half) — trivial.
4. *Then* re-open the lens for the sweeper + R8-M3 scope.

Each of (1)+(2)+(3) is sub-30-minutes work; together they would move
the score floor to ~88-89 and validate the plateau projection.

---

## 8. Score

**85 / 100** (unchanged vs r7: **85**)

### Movement vs r7

- **+0 net.** The only plugin-db commit since r7 (`9e392ba1`) is a
  pure docstring rewrite of the `error.rs` module-level rustdoc. It
  touches lines 7-44 (the `//!` comment block); the `prefix_message`
  function body (lines 370-388) and every `coded_db` / `coded_sql`
  site are byte-identical to r7. **No structural change to the
  migration pipeline.** The docstring accuracy fix is a genuine
  improvement on a different lens (code-critique R8-1) but not on
  this one.
- **F1 (R8-I1), F2 (R8-I2), R8-M1, R8-M2, R8-M3, R8-M4, R8-M5,
  R8-M6 unchanged.** Same dispatch ordering as r7.

### Score components (unchanged from r7)

- **Strict-deploy correctness:** strong; plan/validate/apply
  boundaries crisp; guard `#[must_use]`. **91/100** (unchanged).
- **Lenient/off-deploy correctness:** destructive ops retained in
  `plan.ops` so apply skips them at the loop gate. **75/100**
  (unchanged).
- **Audit-row state machine consistency:** Two orphan classes (F1,
  F2); five `let _ =` DDL sites unchanged. **68/100** (unchanged).
- **Backfill orchestrator:** in-tx progress UPDATE sound (R4 [I41]
  preserved); row lock semantics correct; cross-batch generation
  check intact. R8-M1 residual; R8-M2 SDK-trusted cursor monotonicity
  latent. Terminal transition warn-on-err since r6. **82/100**
  (unchanged).
- **CIC recovery loop:** robust; deterministic SQLSTATE
  classification; bounded retry; partial-index cleanup. **90/100**
  (unchanged).
- **Concurrent `register_model`:** silent-leak variants observable;
  panic-unwind leak window remains (R8-M3). **90/100** (unchanged).
- **Error-helper consistency / dedup:** unchanged at **90/100**. The
  docstring fix at `9e392ba1` is a documentation-accuracy win, not
  a code consistency one.

### Comparison to r7 (85)

**No change.** The r7 → r8 delta is zero on this lens. The plateau
signal is clear: this lens is now blocked on F1/F2 schema-migration
work, not on further mechanical refactoring. Both IMPORTANTs have
been in carry since r2 (7+ rounds, no movement).

The score remains **85/100** for the seventh consecutive review of
this rail (r6 → r7 → r8). Without F1/F2 shipping, the asymptote is
86-87; with them, 90-92.

**Recommendation: pause this lens.** Ship R8-I2 + R8-M1 + the
`tracing::warn!` half of R8-I1 first (combined ~35 LOC), then
re-open. Per round 8 dispatch's plateau check: **further r9 in this
lens without that work landing is approximately +0 value.**
