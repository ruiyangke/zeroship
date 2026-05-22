# plugin-db: Migration / DDL Pipeline Correctness Review (R9)

**HEAD:** `757026e3` (post-r8 cycles 09:30 → 09:47) · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85) · r8 (85/100)

---

## 0. Scope of this round

Per the dispatch brief, the migration-pipeline-relevant delta since
r8 (`9e392ba1`) is **zero code-path commits**. The plugin-db commits
in the window:

| Commit | Module | Migration-pipeline relevance |
|---|---|---|
| `7d0bc4c5` | `exec.rs` | Unify `lazy_init_failed` SDK code on exec.rs cold-init; **doesn't touch migration pipeline files** |
| `389749ca` | `v8_classes/migration.rs`, `v8_classes/migrations.rs` | Rename SDK code `not_configured` → `backend_not_initialized` (2 strings in v8_class shims). Migration pipeline core (`migrations.rs`, `audit.rs`, `orchestrator/*`, `backend/postgres.rs`) untouched |
| `7bd2187e` | `benches/bench_query_build.rs` (new) | Bench harness scaffold for `build_find`/`build_insert`; not pipeline-adjacent |
| `757026e3` | `audit.rs` (1-line preamble), `orchestrator/mod.rs` (5-line preamble), `query.rs`, `error.rs` | Pure docstring closures — the `audit.rs` change drops a stale "(future) backfill" qualifier in the module preamble; the `orchestrator/mod.rs` change corrects a `pub`-vs-`pub(crate)` claim. **Zero emitted-code change** on the migration pipeline. |

Verified via `git diff 9e392ba1 HEAD -- crates/plugin-db/src/migrations.rs crates/plugin-db/src/audit.rs crates/plugin-db/src/orchestrator crates/plugin-db/src/backend/postgres.rs` — the only hunks are two doc-comment rewrites totalling 7 changed lines. No emitted-code byte differs across `audit.rs:181-792`, `migrations.rs:*`, `orchestrator/register_model/*`, `orchestrator/lock_guard.rs`, or `backend/postgres.rs:385-601`.

The two v8_class SDK-code rename hunks (`389749ca`) are at
`v8_classes/migration.rs:269` and `v8_classes/migrations.rs:180` —
that's the JS-facing shim layer (cold-start `backend_not_initialized`
guard), **not** the migration audit/state pipeline. The pipeline only
sees `not_configured` → `backend_not_initialized` as a string-rename in
the cold-start refusal branch *before* any audit row is written. No
state-machine surface affected.

Net structural change to the migration pipeline this round: **zero**.

Re-audit dimensions per the brief: **F1**, **F2**, **F3 / [I41]**,
**R5-M7**, **R5-M8** (cursor monotonicity), **CIC retry budget**,
**concurrent register_model + migration**, **plateau check**.

---

## 1. Audit dimensions — re-verified at HEAD (757026e3)

### 1.1 F1 (orphan Running DDL audit rows) — OPEN, 8+ cycle carry

**Status:** unchanged since r2. Five sites still discard the typed
`Result` of a terminal-transition write via `let _ =`:

| # | File:line | Site |
|---|---|---|
| 1 | `orchestrator/register_model/apply.rs:163-170` | DDL Applied transition |
| 2 | `orchestrator/register_model/apply.rs:178-186` | DDL Failed transition |
| 3 | `backend/postgres.rs:478-486` | CIC `invalid_index_landed` Failed |
| 4 | `backend/postgres.rs:519-528` | CIC `data_violation` Failed |
| 5 | `backend/postgres.rs:558-567` | CIC `transient_retry` / `non_transient_failure` Failed |

Companion gap: the DDL `write_audit_row` INSERT at `audit.rs:283-316`
still omits `owner_session_id` / `last_heartbeat_at` from its column
list (the INSERT column tuple is `(collection, phase, change_class,
change_kind, details, ddl_sql, status, deploy_id, applied_by_kind,
schema_version)`).

The backfill INSERT (`audit.rs:561-602`) and UPDATE
(`audit.rs:525-545`) DO stamp `pg_backend_pid()::text` + `NOW()` into
those columns. DDL/backfill column-coverage asymmetry makes a
heartbeat-driven sweeper impossible to write without first filling the
columns on the DDL rail. **Unchanged from r7/r8.**

### 1.2 F2 (orphan validate-refused Pending) — OPEN, 8+ cycle carry

**Status:** unchanged. `validate.rs:66-95` still writes
`InitialStatus::Pending` per destructive op. Two terminal paths:

- **strict** (`validate.rs:90-92`): `return Err(envelope)`, the
  Pending row is never driven to a terminal status.
- **lenient** (`validate.rs:93-95` fall-through): destructive ops
  retained in `plan.ops`, `apply.rs:203 / 233` skips them, the Pending
  row is never reached again.

Status CHECK at `audit.rs:218-220` still:

```sql
status IN ('pending','running','applied','applied_with_dead_letter',
           'failed','cancelled','rolled_back')
```

— missing `validation_refused`. Even if `validate.rs` wanted to flip
the row to a terminal "refused" state, the CHECK would reject SQLSTATE
23514. **Unchanged from r7/r8.**

### 1.3 F3 / [I41] — CLOSED, re-verified

`exec_commit_batch` (`migrations.rs:439-660`) walked end-to-end against
HEAD:

```
line 462  client = take_lock_client()
line 473  client_exec("BEGIN")
line 487-496  lock_audit_row_for_update                ← FOR UPDATE row lock
line 497-501  if row.status == "cancelled" → rollback_and_return; err_cancelled_mid_run
line 506-509  if row.audit_generation != start_generation → rollback_and_return; err_reset_externally
line 513      for upd in updates_arr: build UPDATE / client_exec
line 585      if !dry_run:
line 586-600    update_backfill_progress              ← inside tx, under row lock
line 606-610  final_sql = "COMMIT" | "ROLLBACK"; client_exec(final_sql)
line 614      if is_done:
line 635-648    finalise_backfill (tracing::warn! on Err)   ← R5-M7 close intact
line 650-654    release_advisory_lock + drop(client) + clear_mig_lock
```

`lock_audit_row_for_update` SQL (`audit.rs:685-688`) unchanged:
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

Byte-identical to r7/r8. Lock release order (line 650-654: release ⇒
drop ⇒ clear_mig_lock) unchanged. **Closed; re-verified.**

### 1.5 R5-M8 / R6-M2 — cursor monotonicity SDK-trusted (latent)

`audit.rs:707-736` (`update_backfill_progress`):

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
| `pg_index.indisvalid = true` | `Ok` (line 458-473) | ✓ `Ok(())` at line 472 |
| `pg_index.indisvalid = false` | `Ok` (lines 475-499) | loop unless `attempt == MAX_RETRIES` → `cic_failed` at 488-499 |
| SQLSTATE in {23505,23502,23503,23514} | `Err`-fatal (501-539) | ✓ `unique_violation` envelope at 530-538 |
| SQLSTATE in {40P01,53100,53200} | `Err`-transient (541-583) | loop unless `attempt == MAX_RETRIES` → `cic_failed` at 570-583 |
| Other SQLSTATE | `Err`-other (548-583) | ✓ `cic_failed` (no retry) — line 570 `!transient` branch |
| Loop exits without terminal return | line 593-600 | `cic_configuration` (defence-in-depth tripwire) |

Per attempt, partial-index cleanup always runs (lines 487 / 529 /
569: `DROP INDEX CONCURRENTLY IF EXISTS`); a Running audit row and a
best-effort Failed terminal write fire on every retry (these are 3 of
the 5 F1-family sites).

`MAX_RETRIES = 3` is unchanged; classification table unchanged;
fallback at 593-600 unchanged. **Sound; unchanged from r7/r8.**

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
- `backend/postgres.rs:109-136` — `acquire_advisory_lock` issues
  `pg_advisory_lock(hashtext($1)::int4, hashtext($2)::int4)`
  (blocking).
- `backend/postgres.rs:138-155` — `try_acquire_advisory_lock` issues
  `pg_try_advisory_lock(...)` (non-blocking, returns bool).

Joint hashtext-space collision: ~1/2⁶⁴ per (app, name) pair. On
collision, the failure mode is: `register_model` queues (blocking
acquire); `migrations.exec_begin` receives `false` from
`pg_try_advisory_lock` and returns `err_already_running()` — coded
SDK error, no hang. **Effectively zero risk; unchanged.**

### 1.8 Wire-shape preservation (deeefe18 + cbbc9059) — re-verified

`coded_db` at `migrations.rs:84-93` is byte-identical to r8 — same
`prefix_message` delegation. `prefix_message` at `error.rs:370-388`
(actual lines may shift by ±10 due to the post-r8 doc-only commits
but the function body is unchanged). The 18 `coded_db` call sites
across `migrations.rs` continue to preserve `.code` and prepend the
lifecycle phrase to `.message` only.

The post-r8 v8_class SDK-code rename (`389749ca`,
`not_configured` → `backend_not_initialized`) is at the JS-facing
boundary, **not** at the migration audit-row state machine. It widens
SDK code unification (twin of `7d0bc4c5`'s `lazy_init_failed`
consolidation) but doesn't touch the migration pipeline's `.code`
discipline. **Closed; re-verified.**

### 1.9 `__zeroship_migrations` table schema — no changes

`ensure_audit_table_exists` (`audit.rs:181-261`) is byte-identical to
r7/r8. 23 columns; `audit_generation BIGINT NOT NULL DEFAULT 0` added
idempotently via `ALTER … ADD COLUMN IF NOT EXISTS`. Status CHECK
still lacks `validation_refused`. Phase CHECK
(`'ddl','validation','backfill','audit'`) unchanged.

---

## 2. Pipeline diagram at HEAD (r9)

Functionally identical to r7 and r8. Reproduced unchanged for cross-
round reference:

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

## 3. R9 findings

### CRITICAL

(none)

### IMPORTANT

**[R9-I1] crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186 — F1: Orphan `Running` DDL audit rows (carry-forward from R8-I1 / R7-I1 / R6-I1 / R5-I1 / r2).**

  Why: data loss / stall / cross-app impact:
    No data loss; no stall (the OrchestratorLockGuard always releases
    on observable Err paths). Observability rot — per-deploy at least
    one stale `Running` row possible after a transient terminal-
    UPDATE failure, indistinguishable from a real in-flight DDL by an
    operator scan. The DDL `write_audit_row` INSERT
    (`audit.rs:283-316`) further omits `owner_session_id` /
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
       demote to `failed`. Separate work item.

  Verification:
    `apply.rs:163-170` (Applied `let _ =`); `apply.rs:178-186`
    (Failed `let _ =`); `audit.rs:283-316` (DDL INSERT — column list
    `(collection, phase, change_class, change_kind, details, ddl_sql,
    status, deploy_id, applied_by_kind, schema_version)` — no
    session/heartbeat columns); `audit.rs:525-545`
    (`set_backfill_running` DOES stamp `pg_backend_pid()::text +
    NOW()` — asymmetry); `audit.rs:561-602`
    (`insert_backfill_running` DOES stamp pid+heartbeat — asymmetry).

**[R9-I2] crates/plugin-db/src/orchestrator/register_model/validate.rs:66-95 — F2: Orphan `Pending` validate-refused audit rows (carry-forward from R8-I2 / R7-I2 / R6-I2 / R5-I2 / r2).**

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
       variant needed.

  Verification:
    `validate.rs:66-95` (Pending write site at line 77,
    `InitialStatus::Pending`); `validate.rs:90-92` (strict returns
    Err with no terminal transition); `validate.rs:93-95` (lenient
    falls through with destructive ops retained); `apply.rs:203 +
    233` (destructive skip path — never reaches
    `update_audit_status`); `audit.rs:218-220` (status CHECK
    constraint — `validation_refused` not in IN-list).

### MINOR

**[R9-M1] crates/plugin-db/src/migrations.rs:279-296 / audit.rs:525-545 — `set_backfill_running` lacks generation predicate (carry-forward R8-M1 / R7-M1 / R6-M1 / R5-M1).**

  Why: data loss / stall / cross-app impact:
    No data loss. Transient UI ghost: an operator `migrations.reset`
    between the `find_latest_backfill_row` SELECT at
    `migrations.rs:279-282` and the `set_backfill_running` UPDATE at
    `migrations.rs:293-296` flips the row's `audit_generation` from
    `g0` to `g0+1`. `set_backfill_running` runs
    `UPDATE … SET status = 'running' … WHERE id = $1::bigint`
    (`audit.rs:525-545`) and overwrites the reset's `'pending'`. The
    reset isn't *lost* — the next `commit_batch`'s FOR-UPDATE +
    generation check (`migrations.rs:506-509`) catches it and surfaces
    `err_reset_externally` — but the row briefly shows
    `status='running'` to operator-side audit scans. Self-heals at
    next batch. Cosmetic.

  Fix:
    Add `AND audit_generation = $2::bigint` to
    `set_backfill_running`'s WHERE (`audit.rs:525-545`) and pass
    `row.audit_generation` from `migrations.rs:294`; on 0 rows
    affected, return `err_reset_externally()` directly so the SDK
    mints a fresh wrapper.

  Verification:
    `migrations.rs:279-296` (snapshot-then-set is non-transactional,
    no gen-check passed); `audit.rs:525-545` (UPDATE keyed only by
    id).

**[R9-M2] crates/plugin-db/src/audit.rs:707-736 — `update_backfill_progress` cursor monotonicity SDK-trusted (carry-forward R8-M2 / R7-M2 / R6-M2 / R5-M8).**

  Why: data loss / stall / cross-app impact:
    Narrow. A buggy SDK or rolled-back-batch-then-retry path that
    re-issues `commit_batch` with a stale `nextCursor` would silently
    rewind the cursor. Risk-of-reprocessing, not risk-of-skip
    (per-row migrations are idempotent — B1 contract). No data loss.

  Fix:
    Add `AND (validate_cursor IS NULL OR validate_cursor < $2::bigint)`
    to the WHERE; on 0 rows affected, surface
    `migration_cursor_rewind` coded error so the SDK can re-snapshot.

  Verification:
    `audit.rs:715-723` (UPDATE keyed only by id; no monotonicity
    predicate).

**[R9-M3] crates/plugin-db/src/orchestrator/lock_guard.rs:212-240 — Drop-path panic still mitigated only by logs (carry-forward R8-M3 / R7-M3 / R6-M3 / R5-M3).**

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

**[R9-M4] crates/plugin-db/src/backend/postgres.rs:459-462 — `pg_index` validity check uses string-interpolated `indexrelid` (carry-forward R8-M4 / R7-M4 / R6-M4 / R5-M4).**

  Why:
    `SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass`
    interpolates `qualified_idx` via `replace('\'', '\'\'')`. Index
    names come from `query::build_*_indexes` (caller-controlled and
    upstream-validated); still belt-and-braces.

  Fix:
    `query_text_params("… WHERE indexrelid = $1::regclass",
    &[qualified_idx.as_str()])`.

  Verification:
    `postgres.rs:459-462`.

**[R9-M5] crates/plugin-db/src/v8_classes/migration.rs + crates/plugin-db/src/migrations.rs (exec_cancel) — operator path strands the in-flight worker's lock client (carry-forward R8-M5 / R7-M5 / R6-M5 / R5-M5).**

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
    Not actively a worsening. Same posture as r7/r8.

**[R9-M6] crates/plugin-db/src/backend/postgres.rs:593-600 — CIC retry-exhaustion fallback variant divergence (carry-forward R8-M6 / R7-M6 / R6-M6 / R5-M6).**

  Why:
    Loop-exit-without-terminal returns
    `DbError::Configuration { code: "cic_configuration" }` while
    every other terminal arm returns
    `SchemaRefused { code: "cic_failed" }`. Cosmetic; defence-in-
    depth tripwire. Unreachable in current code.

  Fix:
    Harmonise the fallback code to `cic_failed` for SDK consistency,
    or keep the divergent code with a documented "invariant breach"
    tag (the current state).

---

## 4. Stage invariants at HEAD (r9 — identical to r7/r8)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Guard held (`#[must_use]`); schema + audit table exist; `schema_version` monotonic | Err → `guard.release().await; return Err` |
| **plan** | Lock held | `Vec<DiffOp>` classified ([I28]); `compute_diff` pure | Err → caller releases |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows NOT terminalised** (R9-I2 / F2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows Running→Applied/Failed; **terminal UPDATE errors silently discarded** (R9-I1 / F1); `check_destructive_invariant` traps misclassified Drops | Err → release (apply.rs:226), then `pass1?` propagates |
| **apply pass 2** | Lock released | CIC ops run idempotently (`IF NOT EXISTS`); retry loop with `pg_index` indisvalid check + audited retry log (4 attempts; deterministic SQLSTATE classification) | terminal → `SchemaRefused { code: "cic_failed" }`; fallback → `cic_configuration` (R9-M6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`set_backfill_running` lacks gen predicate** (R9-M1); `insert_backfill_running` raises `DbError::Internal` on empty RETURNING via `first_row_or_internal` | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held | BEGIN; row FOR UPDATE inside tx; generation check inside tx; per-row UPDATEs; **`update_backfill_progress` inside tx under row lock** (R4 [I41] preserved); COMMIT/ROLLBACK; is_done → `finalise_backfill` (`tracing::warn!` on Err — R5-M7 closed) + release + drop | dry-run → ROLLBACK; partial-failure → `rollback_and_return` |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release in-flight worker's mig advisory lock** (R9-M5) | benign for audit row; isolate wedged until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — closed at r4 |
| **OrchestratorLockGuard::release** | Guard live | `pg_advisory_unlock` issued (best-effort, logs `tracing::warn!` on SQL error); released flag set AFTER unlock await; returns unlocked PooledClient | SQL errors logged not swallowed silently |
| **OrchestratorLockGuard::drop** (panic / forgotten release) | n/a | `tracing::error!` with "leak:" prefix + diagnostic checklist (808a32af); PooledClient returns with lock held; auto-release on backend session close | Documented catastrophic fallback (R9-M3) |

---

## 5. Audit-row terminal-transition reliability (r9 re-tally)

Sites that discard a typed `Result` from a state-transition write
(the F1 family) — **unchanged from r7/r8: 5 sites**.

1. `apply.rs:163-170` — DDL Applied transition (F1, R9-I1).
2. `apply.rs:178-186` — DDL Failed transition (F1, R9-I1).
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
No cross-discipline interference. **Unchanged from r7/r8.**

---

## 7. Plateau check

The brief asks: has the plateau projection from r8 (86-87 without
F1/F2; 90-92 with) changed?

**Read of the round:** since r8 the migration-pipeline call graph has
seen **zero structural change**. The two adjacent SDK-code unification
commits (`7d0bc4c5`, `389749ca`) tightened the JS-facing
`.code` discipline but neither touches the audit-row state machine,
the DDL pass, or the backfill orchestrator. The bench scaffold
(`7bd2187e`) and four documentation-only closures (`757026e3`) are
likewise pipeline-orthogonal.

**Plateau status:**

- **r8 projection holds.** Without F1+F2 work landing, the asymptote
  remains **86-87**. There is one further +1 available from R9-M1
  (5 LOC, fully local) and another +1 available from R9-M4 (3 LOC,
  parameter-binding swap), totalling a possible **87** in this lens
  before plateau.
- After F1 (audit-row state machine 68→78) and F2 (validate stage
  CHECK + ValidationRefused), the lens unlocks to **90-92**: the
  remaining gap concentrates in R9-M3 (catch_unwind) and R9-M2
  (cursor monotonicity), both more disruptive next-stratum work.
- Beyond ~92, further migration-pipeline review at this lens needs
  to widen scope to include the **cross-isolate / cross-pod**
  invariants (e.g. how does the sweeper know which DDLs to demote
  when multiple workers concurrently mark Running?) which isn't
  pure-code work — it's distributed-systems design.

**Round-over-round signal:**

- r2 → r3: +0 (re-verification round; same findings)
- r3 → r4: +1 ([I41] cursor-in-tx close)
- r4 → r5: +1 (lock-guard extraction)
- r5 → r6: +1 (R5-M7 finalise_backfill warn-on-err close)
- r6 → r7: +1 (`coded_db` wired through `prefix_message` —
  error-helper consolidation visible at this lens)
- r7 → r8: **+0** (first non-positive movement since r2)
- r8 → r9: **+0** (second consecutive zero)

Two consecutive +0 rounds against an asymptote of 86-87 without F1/F2
landing confirms the plateau signal r8 first surfaced.

**Recommendation:** the lens has narrowed to two findings that need
landing. The marginal value of an r10 in this lens without F1/F2
shipping is approximately **+0**. Suggest pivoting to:

1. Land R9-I2 (F2 close) — biggest single jump.
2. Land R9-M1 — trivial.
3. Land R9-I1 *partial* (`tracing::warn!` half) — trivial.
4. *Then* re-open the lens for the sweeper + R9-M3 scope.

Each of (1)+(2)+(3) is small (~35 LOC combined); together they would
move the score floor to ~88-89 and validate the plateau projection.

---

## 8. Score

**85 / 100** (unchanged vs r8: **85**; unchanged vs r7: **85**)

### Movement vs r8

- **+0 net.** The plugin-db commits since r8 (`7d0bc4c5`, `389749ca`,
  `7bd2187e`, `757026e3`) are SDK-code unification at the v8_class
  boundary, a bench scaffold, and four documentation-only closures.
  Zero emitted-code bytes change in `migrations.rs`, `audit.rs`,
  `orchestrator/register_model/*`, `orchestrator/lock_guard.rs`, or
  `backend/postgres.rs:385-601`. **No structural change to the
  migration pipeline.**
- **F1 (R9-I1), F2 (R9-I2), R9-M1, R9-M2, R9-M3, R9-M4, R9-M5, R9-M6
  unchanged.** Same dispatch ordering as r8.

### Score components (unchanged from r7/r8)

- **Strict-deploy correctness:** strong; plan/validate/apply
  boundaries crisp; guard `#[must_use]`. **91/100** (unchanged).
- **Lenient/off-deploy correctness:** destructive ops retained in
  `plan.ops` so apply skips them at the loop gate. **75/100**
  (unchanged).
- **Audit-row state machine consistency:** Two orphan classes (F1,
  F2); five `let _ =` DDL sites unchanged. **68/100** (unchanged).
- **Backfill orchestrator:** in-tx progress UPDATE sound (R4 [I41]
  preserved); row lock semantics correct; cross-batch generation
  check intact. R9-M1 residual; R9-M2 SDK-trusted cursor monotonicity
  latent. Terminal transition warn-on-err since r6. **82/100**
  (unchanged).
- **CIC recovery loop:** robust; deterministic SQLSTATE
  classification; bounded retry; partial-index cleanup. **90/100**
  (unchanged).
- **Concurrent `register_model`:** silent-leak variants observable;
  panic-unwind leak window remains (R9-M3). **90/100** (unchanged).
- **Error-helper consistency / dedup:** unchanged at **90/100**. The
  v8_class SDK-code rename at `389749ca` is a JS-facing wire fix, not
  a migration-pipeline state-machine change.

### Comparison to r8 (85) and r7 (85)

**No change.** The r8 → r9 delta is zero on this lens. Two
consecutive zero rounds confirm the plateau signal r8 first surfaced.
This lens is blocked on F1/F2 schema-migration work, not on further
mechanical refactoring. Both IMPORTANTs have been in carry since r2
(8+ rounds, no movement).

The score remains **85/100** for the eighth consecutive review of
this rail (r6 → r7 → r8 → r9). Without F1/F2 shipping, the asymptote
remains 86-87; with them, 90-92.

**Honest assessment:** r9 was, as r8 predicted, a +0 round on this
lens. The migration pipeline is in a steady state where every fresh
review re-discovers the same five F1 sites, the same F2 orphan path,
the same R9-M[1-6] minors. The next meaningful movement on this lens
requires one of:

1. **F2 (R9-I2)** to land (status CHECK + `ValidationRefused`
   variant + flip Pending writes to Running→Refused). This is the
   highest leverage single change and unblocks the audit-row state
   machine from 68 → ~78.
2. **F1 (R9-I1)** partial close — extend the five `let _ =` sites to
   `if let Err = ... { tracing::warn!(...); }`. Trivial to ship;
   moves the lens floor 1-2 points.
3. **R9-M1** trivial close — gen-predicate on `set_backfill_running`.
   ~5 LOC.

**Recommendation: pause this lens.** Ship at least R9-M1 + the
`tracing::warn!` half of R9-I1 before the next iteration; if F2
(R9-I2) ships alongside, the lens will move materially (+3 to +5 to
floor). Per r8 dispatch's plateau check, validated by this r9: further
review at this lens without that work landing is approximately
**+0 value per round**, and we now have two consecutive rounds of
empirical evidence (r8, r9) for that.
