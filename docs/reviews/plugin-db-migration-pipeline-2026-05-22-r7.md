# plugin-db: Migration / DDL Pipeline Correctness Review (R7)

**HEAD:** post-`f6043126` · **Date:** 2026-05-22 (cycle ~07:30+) · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84/100)

---

## 0. Scope of this round

Per the dispatch brief, the plugin-db delta since r6 (`d2e7e22` HEAD)
is two commits:

| Commit | Module | Migration-pipeline relevance |
|---|---|---|
| `deeefe18` | `migrations.rs` (`coded_db` helper) | Architecture r8 M11 dedup — `coded_db` now delegates to `crate::error::prefix_message` instead of open-coding the variant-walk. Verify wire-shape preservation at all `coded_db` call sites. |
| `f6043126` | `replication.rs` + `auth/session.rs` | SQLSTATE-typed substring replacements; tests for `classify_detail_token`. **Not on the migration-pipeline rail.** Verify zero impact. |

Net migration-pipeline structural change: **one** commit (`deeefe18`),
and it is a behaviour-preserving refactor that consolidates the
variant-walk into the shared helper at `crate::error::prefix_message`.

Re-audit dimensions from the brief: **F1**, **F2**, **F3 / [I41]**,
**R5-M7** (finalise_backfill warn), **R5-M8** (cursor monotonicity),
**deeefe18 wire-shape impact on coded_db call sites**, **CIC retry
budget**, **concurrent register_model + migration**.

---

## 1. Audit dimensions per dispatch brief

### 1.1 Carry-overs — status check at HEAD

| ID | r6 status | HEAD status | Evidence |
|---|---|---|---|
| **F1** (orphan DDL `Running` audit rows) | OPEN | **OPEN, unchanged** | `apply.rs:163-170` (Applied `let _ =`) and `apply.rs:178-186` (Failed `let _ =`) both untouched since r6. CIC trio at `postgres.rs:478-486 / 519-528 / 558-567` likewise untouched. DDL `write_audit_row` INSERT at `audit.rs:286-308` still omits `owner_session_id` / `last_heartbeat_at` (DDL/backfill asymmetry). |
| **F2** (orphan validate-refused `Pending`) | OPEN | **OPEN, unchanged** | `validate.rs:69-87` writes `InitialStatus::Pending` per destructive op. Strict path returns Err (line 91); lenient path retains destructive ops in `plan.ops` so apply skips them (`apply.rs:203, 233`). Status CHECK at `audit.rs:218-220` still lacks `validation_refused`. |
| **F3** / [I41] (post-COMMIT reset clobber) | CLOSED at r4 | **CLOSED, re-verified** | `migrations.rs:588-603` — `update_backfill_progress` runs under the FOR UPDATE row lock (`lock_audit_row_for_update` at `audit.rs:680-700`), inside the same tx as data UPDATEs, BEFORE `COMMIT` at line 610. Re-walked end-to-end below. |
| **R5-M7** (finalise_backfill silent Err) | CLOSED at r6 | **CLOSED, re-verified** | `migrations.rs:638-651` — `if let Err(e) = backend.finalise_backfill(...).await { tracing::warn!(...) }`. Unchanged since `51ced4a0`. |
| **R5-M8** (cursor monotonicity) | OPEN (SDK-trusted) | **OPEN, unchanged** | `audit.rs:715-723` — `update_backfill_progress` UPDATE keyed only by id; no `validate_cursor < $2::bigint` predicate. |

### 1.2 deeefe18 — wire-shape preservation at `coded_db` call sites

This was the central r7 dispatch question. The brief lists "all 4
coded_db call sites in migrations.rs"; the actual count at HEAD is 18
sites (Grep `coded_db` in migrations.rs).

**Before (pre-`deeefe18`):**

```rust
fn coded_db(context: &str, e: crate::error::DbError) -> OpError {
    let mut db_err = e;
    match &mut db_err {
        crate::error::DbError::UniqueViolation { message }
        | crate::error::DbError::FkViolation { message }
        | crate::error::DbError::NotNullViolation { message }
        | crate::error::DbError::CheckViolation { message }
        | crate::error::DbError::Serialization { message }
        | crate::error::DbError::LockContention { message }
        | crate::error::DbError::Transient { message }
        | crate::error::DbError::Internal { message } => {
            *message = format!("{context}: {message}");
        }
        _ => {}
    }
    db_err.to_op_error()
}
```

**After (`deeefe18`, current):**

```rust
fn coded_db(context: &str, e: crate::error::DbError) -> OpError {
    let mut db_err = e;
    crate::error::prefix_message(&mut db_err, &format!("{context}: "));
    db_err.to_op_error()
}
```

**Shared `prefix_message` body (`error.rs:332-350`):**

```rust
pub(crate) fn prefix_message(err: &mut DbError, prefix: &str) {
    match err {
        DbError::UniqueViolation { message }
        | DbError::FkViolation { message }
        | DbError::NotNullViolation { message }
        | DbError::CheckViolation { message }
        | DbError::Serialization { message }
        | DbError::LockContention { message }
        | DbError::Transient { message }
        | DbError::Internal { message } => {
            *message = format!("{prefix}{message}");
        }
        _ => {}
    }
}
```

**Bit-exact match.** The 8 prefix-eligible variants are identical
between the two. The `_ => {}` arm covers the same set
(`ValidationFailed | Configuration | Coded | SchemaRefused`), all of
which carry SDK-contracted wire payloads that the SDK parses verbatim
(e.g. `SchemaRefused.envelope_json`). The `to_op_error()` conversion
runs after the same mutation. The `.code` derivation at the V8
boundary therefore stays SQLSTATE-classification-driven for the
prefix-eligible variants and structurally-coded for the others. **Zero
wire-shape drift.**

The contract is unit-tested at `error.rs:701-757`
(`prefix_message_preserves_variant_and_code`): all 8 variants assert
that (a) the body acquires the prefix, (b) the original tail is
preserved, and (c) `to_op_error().kind.CodedError.code` matches the
expected wire code (`unique_violation`, `fk_violation`,
`not_null_violation`, `check_violation`, `serialization_failure`,
`lock_not_available`, `transient`, `internal`). The
`prefix_message_leaves_structured_variants_alone` test (line 766) is
the complement.

Walked all 18 sites — the contract holds at each:

| Line | Context phrase | Surface |
|---|---|---|
| 247 | `"create schema"` | exec_begin |
| 267 | `"advisory_lock query"` | exec_begin |
| 279 | `"migration reset"` | exec_begin (reset flag) |
| 285 | `"migration lookup"` | exec_begin |
| 299 | `"migration set running"` | exec_begin (R6-M1 site) |
| 319 | `"migration insert"` | exec_begin |
| 400 | `"status read"` | exec_fetch_batch |
| 426 | `"migration fetch"` | exec_fetch_batch |
| 478 | `"BEGIN"` | exec_commit_batch |
| 497 | `"audit lock"` | exec_commit_batch (FOR UPDATE) |
| 574 | `"migration row UPDATE (id={id})"` | exec_commit_batch (per-row UPDATE) |
| 601 | `"audit row update"` | exec_commit_batch (progress) |
| 612 | `"migration {COMMIT|ROLLBACK}"` | exec_commit_batch (final) |
| 680 | `"migration status read"` | exec_status |
| 722 | `"migration cancel lookup"` | exec_cancel |
| 733 | `"migration cancel update"` | exec_cancel |
| 758 | `"migration reset"` | exec_reset |

All 18 retain the operator-facing `"<context>: db: ..."` prefix shape
and SQLSTATE-derived `.code`. **deeefe18 is shape-clean across the
migration pipeline.**

### 1.3 f6043126 — non-impact on migration pipeline

`f6043126` touches `replication.rs:213, 257` (substring-match → SqlState
predicate against `DUPLICATE_OBJECT` and
`OBJECT_NOT_IN_PREREQUISITE_STATE`) and adds tests for the
`classify_detail_token` helper extracted from `auth/session.rs`.

Neither file is on the migration-pipeline call graph:

- `replication.rs` is the WAL/publication/slot bootstrap path, called
  from `v8_classes/replication.rs`. The migration pipeline never
  reaches publication setup.
- `auth/session.rs` is the user-session-init path (`session.start`,
  HMAC bootstrap). Unrelated to `register_model` /
  `migrations.{begin,fetchBatch,commitBatch}`.

Grepped `classify_detail_token` and the two replication sites — no
imports from any of: `migrations.rs`, `orchestrator/`, `apply.rs`,
`validate.rs`, `diff.rs`, `audit.rs`. **Zero migration-pipeline
impact.** Confirmed.

### 1.4 R5-M7 closure re-verified

```rust
// migrations.rs:633-651
// Discarding finalise_backfill errors silently can leave the
// audit row stuck in Running (migration-pipeline r5 R5-M7,
// F1 family). Log via tracing::warn so operators see the
// stall; we still continue with lock release because the row
// state is already as-good-as-it-gets at this point.
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

The R5-M7 close is identical at HEAD — `51ced4a0` hasn't moved. Lock
release order (release_advisory_lock → drop(client) → clear_mig_lock)
also unchanged at lines 653-657. **Closed; re-verified.**

### 1.5 F3 / [I41] closure re-verified end-to-end

`exec_commit_batch` at HEAD (`migrations.rs:442-663`):

```
take_lock_client                                  (465)
client_exec("BEGIN")                              (476)
lock_audit_row_for_update                         (490)  ← FOR UPDATE
  if row.status == "cancelled"  → rollback_and_return; err_cancelled_mid_run  (501-504)
  if row.audit_generation != start_generation → rollback_and_return; err_reset_externally  (509-512)
for upd in updates_arr:
  validate shape / build UPDATE / client_exec     (572)
if !dry_run:
  update_backfill_progress(audit_id, ...)         (589-602)  ← INSIDE the tx, under row lock
client_exec("COMMIT"|"ROLLBACK")                  (610)
if is_done:
  finalise_backfill (tracing::warn! on Err)       (638-651)
  release_advisory_lock + drop(client)            (653-656)
  clear_mig_lock                                  (657)
```

`lock_audit_row_for_update` SQL (`audit.rs:686-688`):

```
SELECT status, audit_generation
  FROM "{app_id}"."__zeroship_migrations"
  WHERE id = $1::bigint
  FOR UPDATE
```

The FOR UPDATE row lock survives until `COMMIT`/`ROLLBACK`. Any
concurrent `migrations.reset(...)` on another connection serialises
behind it. The generation-mismatch check at line 509 fails fast if
the reset beat us to the FOR UPDATE. **[I41] closed; verified.**

### 1.6 R5-M8 / R6-M2 — cursor monotonicity still SDK-trusted

`audit.rs:715-723`:

```sql
UPDATE "{app_id}"."__zeroship_migrations"
   SET validate_cursor = $2::bigint,
       dead_letter_pks = $3::jsonb,
       details = jsonb_set(COALESCE(details, '{}'::jsonb), '{processed}', to_jsonb($4::bigint)),
       last_heartbeat_at = NOW(),
       updated_at = NOW()
 WHERE id = $1::bigint
```

No `validate_cursor < $2::bigint` predicate; the SDK is trusted to
issue monotonically-advancing `nextCursor`. Per-row migration
idempotence (B1 contract) means a rewind costs reprocessing, not
data loss. **Latent; SDK-trusted; unchanged.**

### 1.7 CIC retry budget — re-walked

`create_index_with_recovery_audited` at `postgres.rs:385-601`.

`MAX_RETRIES = 3` → loop `for attempt in 0..=MAX_RETRIES` (4 iterations).
Per attempt:

1. `pool.query_text_params(&spec.sql, &empty).await` (line 455).
2. **Ok path** (lines 458-499):
   - `SELECT indisvalid FROM pg_index WHERE indexrelid = '<qualified_idx>'::regclass`
     (lines 459-462; R6-M4 string-interpolation site — unchanged).
   - Valid → `Ok(())`. ✓
   - Invalid → `log_retry("invalid_index_landed", ...)` Running audit row
     (476-477), best-effort transition to Failed (478-486 — `let _ =`),
     `DROP INDEX CONCURRENTLY IF EXISTS` (487), final-attempt check
     (488) → `cic_failed` envelope or loop.
3. **Err path — fatal SQLSTATEs** (`UNIQUE_VIOLATION`,
   `NOT_NULL_VIOLATION`, `FOREIGN_KEY_VIOLATION`, `CHECK_VIOLATION` —
   lines 503-509): immediate audit + drop + terminal `unique_violation`
   envelope. No retry. ✓
4. **Err path — transient SQLSTATEs** (`T_R_DEADLOCK_DETECTED`,
   `DISK_FULL`, `OUT_OF_MEMORY` — lines 541-546): audit + drop + loop
   unless on final attempt → terminal `cic_failed` envelope. ✓
5. **Err path — anything else**: audit + drop + immediate terminal
   `cic_failed` envelope. No retry.

Fallback at line 593-600 (`DbError::Configuration { code:
"cic_configuration" }`) is the loop-exit-without-terminal tripwire —
unreachable in current code. **R6-M6 stays as cosmetic.**

**Retry budget unchanged since r6; sound.** Four attempts; SQLSTATE-
classified; partial-index cleanup on every failure; deterministic
terminal returns. Each retry writes an `index_retry` Running audit row
and a best-effort Failed terminal (3 of the 5 F1 family sites live
inside this loop).

### 1.8 Concurrent register_model + migration — disjoint lock keys verified

| Caller | Key1 (namespace) | Key2 (tag) | Mode |
|---|---|---|---|
| `register_model` | `hashtext("zs_reg:<app>")::int4` | `hashtext("register_model")::int4` | **blocking** `pg_advisory_lock` |
| `migrations.exec_begin` | `hashtext("zs_mig:<app>")::int4` | `hashtext(<name>)::int4` | **non-blocking** `pg_try_advisory_lock` |

Evidence:

- `orchestrator/register_model/bootstrap.rs:59-63` —
  `fn lock_key(app_id) -> String { format!("zs_reg:{app_id}") }` +
  `LOCK_TAG = "register_model"`.
- `migrations.rs:263` — `let lock_key = format!("zs_mig:{app_id}");`
  + `name` as tag.

Joint collision probability: ~1/2⁶⁴ per (X, Y, name) triple.
Effectively impossible.

Even on a collision: `register_model` would queue (blocking acquire);
`migrations.exec_begin` would receive `false` from
`pg_try_advisory_lock` and surface `err_already_running()` (coded
SDK error). Better failure mode than hang. **No collision risk in
practice. Unchanged since r5.**

### 1.9 `__zeroship_migrations` table schema — no changes

`ensure_audit_table_exists` at `audit.rs:181-261`. Schema unchanged
from r6: 23 columns including `audit_generation BIGINT NOT NULL
DEFAULT 0` (added idempotently via `ALTER TABLE … ADD COLUMN IF NOT
EXISTS` at audit.rs:234-240). Status CHECK at lines 218-220 still
lacks `validation_refused`. Phase CHECK at lines 212-214 unchanged
(`ddl/validation/backfill/audit`).

---

## 2. Pipeline diagram at HEAD (r7)

Functionally identical to r6 — deeefe18 is a behaviour-preserving
helper refactor.

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
                        insert_backfill_running OR set_backfill_running ← no gen-check (R7-M1 / R6-M1)
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

## 3. R7 findings

### CRITICAL

(none)

### IMPORTANT

**[R7-I1] crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186 — F1: Orphan `Running` DDL audit rows (carry-forward from R6-I1 / R5-I1 / r2).**

  Why: data loss / stall / cross-app impact:
    No data loss; no stall (the OrchestratorLockGuard always releases
    on observable Err paths). Operator-state observability rot —
    per-deploy at least one stale `Running` row possible after a
    transient terminal-UPDATE failure, indistinguishable from a real
    in-flight DDL by an operator scan. The DDL `write_audit_row`
    INSERT (audit.rs:286-308) further omits `owner_session_id` /
    `last_heartbeat_at`, so a future heartbeat-driven sweeper has no
    column to key on for the DDL rail. Blocks any future drift-
    detection / manual-approve UX.

  Fix:
    1. Add `owner_session_id = pg_backend_pid()::text,
       last_heartbeat_at = NOW()` to the DDL INSERT column list
       (audit.rs:286-308).
    2. Replace each `let _ = update_audit_status(...).await;` with
       `if let Err(e) = ... { tracing::warn!(audit_id = id, error = %e,
       "audit: terminal-transition update failed; row stays Running
       until sweeper reaps"); }`. Five sites: apply.rs:163-170 (DDL
       Applied), apply.rs:178-186 (DDL Failed), postgres.rs:478-486
       (CIC invalid_index_landed Failed), postgres.rs:519-528 (CIC
       data_violation Failed), postgres.rs:558-567 (CIC
       transient_retry / non_transient_failure Failed). The R5-M7
       close at migrations.rs:638-651 is the in-tree model.
    3. Ship a heartbeat-driven sweeper: scan
       `phase='ddl' AND status='running' AND (last_heartbeat_at IS
       NULL OR last_heartbeat_at < NOW() - INTERVAL '5 min')` and
       demote to `failed`.

  Verification:
    apply.rs:163-170 (Applied `let _ =`); apply.rs:178-186 (Failed
    `let _ =`); audit.rs:286-308 (DDL INSERT — no session/heartbeat
    columns); audit.rs:531-538 (backfill `set_backfill_running` DOES
    stamp pid+heartbeat — asymmetry); audit.rs:577-585
    (`insert_backfill_running` DOES stamp pid+heartbeat — asymmetry).

**[R7-I2] crates/plugin-db/src/orchestrator/register_model/validate.rs:66-95 — F2: Orphan `Pending` validate-refused audit rows (carry-forward from R6-I2 / R5-I2 / r2).**

  Why: data loss / stall / cross-app impact:
    No data loss; no stall. Pure observability pollution — the audit
    table grows unboundedly with phantom Pending rows on every strict
    refusal and every lenient destructive-skip. The status CHECK
    constraint (audit.rs:218-220) does not include
    `validation_refused`, so even if validate.rs wanted to emit a
    terminal row it would hit SQLSTATE 23514.

  Fix:
    1. Extend `__zeroship_migrations_status_chk` (audit.rs:218-220) to
       include `validation_refused`. Use `ALTER TABLE … DROP
       CONSTRAINT … / ADD CONSTRAINT …` inside `ensure_audit_table`
       (idempotent via try/catch or `DO $$ … $$`).
    2. Add `TerminalStatus::ValidationRefused` to
       `audit::TerminalStatus` with `as_sql() = "validation_refused"`.
    3. In `validate.rs:67-88` change the Pending-write to a
       Running-write + immediate
       `update_audit_status(ValidationRefused, ...)` — mirrors the
       apply-layer state-machine pattern; no new `InitialStatus`
       variant needed.

  Verification:
    validate.rs:69-87 (Pending write site, `InitialStatus::Pending`);
    validate.rs:90-92 (strict returns Err with no terminal transition);
    validate.rs:93-94 (lenient falls through with destructive ops
    retained); apply.rs:203 + 233 (destructive skip path — never
    reaches `update_audit_status`); audit.rs:218-220 (status CHECK
    constraint — `validation_refused` not in the IN-list).

### MINOR

**[R7-M1] crates/plugin-db/src/migrations.rs:296-299 — `set_backfill_running` lacks generation predicate (carry-forward R6-M1 / R5-M1).**

  Why: data loss / stall / cross-app impact:
    No data loss. Transient UI ghost: an operator `migrations.reset`
    between the SELECT at line 282 and the `set_backfill_running`
    UPDATE at line 297 flips the row's `audit_generation` from `g0`
    to `g0+1`. Our `set_backfill_running` runs `UPDATE … status =
    'running' WHERE id = $1::bigint` (audit.rs:530-538) and overwrites
    the reset's `'pending'`. The reset isn't *lost* — the next
    `commit_batch`'s FOR-UPDATE + generation check (migrations.rs:509)
    catches it and surfaces `err_reset_externally` — but the row
    briefly shows `status='running'` to operator-side audit scans.
    Self-heals at next batch. Cosmetic.

  Fix:
    Add `AND audit_generation = $2::bigint` to
    `set_backfill_running`'s WHERE (audit.rs:530-538) and pass
    `row.audit_generation` from migrations.rs:297; on 0 rows
    affected, return `err_reset_externally()` directly so the SDK
    mints a fresh wrapper. ~5 LOC.

  Verification:
    migrations.rs:282-299 (snapshot-then-set is non-transactional,
    no gen-check passed); audit.rs:530-538 (UPDATE keyed only by id).

**[R7-M2] crates/plugin-db/src/audit.rs:707-736 — `update_backfill_progress` cursor monotonicity SDK-trusted (carry-forward R6-M2 / R5-M8).**

  Why: data loss / stall / cross-app impact:
    Narrow. A buggy SDK or rolled-back-batch-then-retry path that
    re-issues `commit_batch` with a stale `nextCursor` would silently
    rewind the cursor. Risk-of-reprocessing, not risk-of-skip
    (per-row migrations are idempotent — B1 contract). No data loss.

  Fix:
    Add `AND (validate_cursor IS NULL OR validate_cursor < $2::bigint)`
    to the WHERE; on 0 rows affected, surface `migration_cursor_rewind`
    coded error so the SDK can re-snapshot. ~4 LOC.

  Verification:
    audit.rs:715-723 (UPDATE keyed only by id).

**[R7-M3] crates/plugin-db/src/orchestrator/lock_guard.rs:212-240 — Drop-path panic still mitigated only by logs (carry-forward R6-M3 / R5-M3).**

  Why: cross-app impact:
    A panic during pipeline execution → Drop logs `tracing::error!`
    "leak:" → pooled client returns to pool with session-scoped
    `pg_advisory_lock(zs_reg:<app>, register_model)` held → every
    subsequent `register_model` for that app stalls until the
    backend session closes (typically pool recycle, tens of
    seconds to minutes). Drop is sync; cannot run async
    `pg_advisory_unlock`.

  Fix:
    Wrap the inner `run_pipeline` body in
    `std::panic::AssertUnwindSafe(...).catch_unwind()` at the dispatch
    boundary; on panic, explicitly run the unlock SQL via a synchronous
    fast-path (or by reissuing a fresh `Client`) before re-raising.
    Documented but unshipped since r4.

  Verification:
    lock_guard.rs:215-237 (Drop body: tracing::error! only; comment
    explicitly acknowledges the limitation).

**[R7-M4] crates/plugin-db/src/backend/postgres.rs:459-462 — `pg_index` validity check uses string-interpolated `indexrelid` (carry-forward R6-M4 / R5-M4).**

  Why:
    `SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass`
    interpolates `qualified_idx` via `replace('\'', '\'\'')`. Index
    names come from `query::build_*_indexes` (caller-controlled and
    upstream-validated); still belt-and-braces.

  Fix:
    `query_text_params("… WHERE indexrelid = $1::regclass",
    &[qualified_idx.as_str()])`. ~3 LOC.

**[R7-M5] crates/plugin-db/src/v8_classes/migration.rs:84-131 + crates/plugin-db/src/migrations.rs:708-742 — `exec_cancel` operator path strands the in-flight worker's lock client (carry-forward R6-M5 / R5-M5).**

  Why: stall:
    `exec_cancel` writes `status='cancelled'` to the audit row via
    the pool (no lock) and returns. The in-flight worker's parked
    `mig_lock` client is not touched. The worker's next `fetchBatch`
    reads the cancelled status and aborts; the lock releases when
    the worker calls `clear_mig_lock` or when the isolate tears
    down. The cancelled migration's `(name, collection)` is wedged
    on that isolate for up to one batch interval before the cancel
    observation triggers the abort + release. No cross-app impact
    (lock is keyed on `(zs_mig:<app>, <name>)`).

  Severity:
    not actively a worsening. Same posture as r6.

**[R7-M6] crates/plugin-db/src/backend/postgres.rs:593-600 — CIC retry-exhaustion fallback variant divergence (carry-forward R6-M6 / R5-M6).**

  Why:
    Loop-exit-without-terminal returns
    `DbError::Configuration { code: "cic_configuration" }` while
    every other terminal arm returns
    `SchemaRefused { code: "cic_failed" }`. Cosmetic; defence-in-depth
    tripwire. Unreachable in current code.

  Fix:
    Harmonise the fallback code to `cic_failed` for SDK consistency,
    or alternatively keep the divergent code with a documented
    "invariant breach" tag (the current state). ~3 LOC.

---

## 4. Stage invariants at HEAD (r7 update)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Guard held (`#[must_use]`); schema + audit table exist; `schema_version` monotonic | Err → `guard.release().await; return Err` |
| **plan** | Lock held | `Vec<DiffOp>` classified ([I28] returns DbError on Err — `compute_diff` is pure) | Err → caller releases |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows NOT terminalised** (R7-I2 / F2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows Running→Applied/Failed; **terminal UPDATE errors silently discarded** (R7-I1 / F1); `check_destructive_invariant` traps misclassified Drops | Err → release (apply.rs:226), then `pass1?` propagates |
| **apply pass 2** | Lock released | CIC ops run idempotently (`IF NOT EXISTS`); retry loop with `pg_index` indisvalid check + audited retry log (4 attempts; deterministic SQLSTATE classification) | terminal → `SchemaRefused { code: "cic_failed" }`; fallback → `cic_configuration` (R7-M6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`set_backfill_running` lacks gen predicate** (R7-M1); `insert_backfill_running` raises `DbError::Internal` on empty RETURNING via `first_row_or_internal` | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held | BEGIN; row FOR UPDATE inside tx; generation check inside tx; per-row UPDATEs; **`update_backfill_progress` inside tx under row lock** (R4 [I41] preserved); COMMIT/ROLLBACK; is_done → `finalise_backfill` (`tracing::warn!` on Err — R5-M7 closed at HEAD) + release + drop | dry-run → ROLLBACK; partial-failure → `rollback_and_return` |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release in-flight worker's mig advisory lock** (R7-M5) | benign for audit row; isolate wedged until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — closed at r4 |
| **OrchestratorLockGuard::release** | Guard live | `pg_advisory_unlock` issued (best-effort, **logs `tracing::warn!` on SQL error**); released flag set AFTER unlock await; returns unlocked PooledClient | SQL errors logged not swallowed silently |
| **OrchestratorLockGuard::drop** (panic / forgotten release) | n/a | `tracing::error!` with "leak:" prefix + diagnostic checklist (808a32af); PooledClient returns with lock held; auto-release on backend session close | Documented catastrophic fallback (R7-M3); reduced silent variant |

---

## 5. Audit-row terminal-transition reliability (r7 re-tally)

Sites that discard a typed `Result` from a state-transition write
(the F1 family) — **unchanged from r6: 5 sites**.

1. `apply.rs:163-170` — DDL Applied transition (F1, R7-I1).
2. `apply.rs:178-186` — DDL Failed transition (F1, R7-I1).
3. `postgres.rs:478-486` — CIC `invalid_index_landed` Failed (F1 family).
4. `postgres.rs:519-528` — CIC `data_violation` Failed (F1 family).
5. `postgres.rs:558-567` — CIC `transient_retry` / `non_transient_failure` Failed (F1 family).

The R5-M7-style fix (`if let Err(e) = ... { tracing::warn!(...); }`)
applies uniformly. The `migrations.rs:638-651` close is the in-tree
model.

OrchestratorLockGuard does not protect these — the audit writes
either go through the pool (DDL Apply, CIC) or through the dedicated
mig_lock client (backfill); the guard's release-on-Err invariant is
orthogonal to per-write error handling.

---

## 6. Concurrent register_model + concurrent migration — re-verified

Lock-key construction unchanged at HEAD:

- `bootstrap.rs:59-63` — register_model uses `lock_key(app_id) =
  "zs_reg:<app>"` + `LOCK_TAG = "register_model"` (blocking).
- `migrations.rs:263` — `let lock_key = format!("zs_mig:{app_id}");`
  + `name` as tag (try-acquire).

Hashtext-space collision: ~1/2⁶⁴ per (app, migration_name) pair.
Even on collision, register_model blocks; migration sees
`pg_try_advisory_lock = false` → `err_already_running()` (coded SDK
error). No cross-discipline interference.

**Unchanged from r6: no collision risk in practice.**

---

## 7. Score

**85 / 100** (+1 vs r6: **84**)

### Movement vs r6

- **+1 for `deeefe18` closing the last per-module `coded_db` variant-
  walk by routing through `crate::error::prefix_message`.** Mechanically
  safe (variant lists are bit-identical pre/post; the shared helper
  carries a dedicated `prefix_message_preserves_variant_and_code` unit
  test exercising all 8 prefix-eligible variants with explicit `.code`
  contract assertions). Lifts the **dedupe / consistency** dimension
  of the codebase by completing the architecture-r8-M11 wave —
  the migration pipeline's error-shaping rail now has a single source
  of truth for variant-walk-prefix-then-convert. No call-site
  behaviour change at any of the 18 sites.
- **+0 for `f6043126`.** Replication/auth-only; the migration-pipeline
  rail is untouched. Verified by Grep of `classify_detail_token` /
  the two replication SqlState predicates.
- **F1 (R7-I1), F2 (R7-I2), R7-M1, R7-M2 unchanged.** Same dispatch
  recommendation as r6: ship F2 first (smallest LOC, biggest
  visibility win), then R7-M1 (~5 LOC), then the `tracing::warn!`
  extension to F1 sites (~15 LOC), then sweeper as a separate work
  item (~30-50 LOC).

### Score components

- **Strict-deploy correctness:** strong; plan/validate/apply
  boundaries crisp; guard `#[must_use]` so accidental drops warn
  at compile time. **91/100** (unchanged).
- **Lenient/off-deploy correctness:** unchanged — destructive ops
  retained in `plan.ops` so apply skips them at the loop gate.
  **75/100**.
- **Audit-row state machine consistency:** R5-M7 close lifted this
  one notch at r6; the `coded_db` consolidation does NOT touch this
  rail (it's a wire-shape refactor, not a state-machine one). Two
  orphan classes (F1, F2) remain; five `let _ =` DDL sites
  unchanged. **68/100** (unchanged from r6).
- **Backfill orchestrator:** in-tx progress UPDATE sound (R4 [I41]
  preserved); row lock semantics correct; cross-batch generation
  check intact. R7-M1 residual; R7-M2 SDK-trusted cursor monotonicity
  latent. Terminal transition warn-on-err since r6. **82/100**
  (unchanged).
- **CIC recovery loop:** robust; deterministic SQLSTATE
  classification; bounded retry; partial-index cleanup. **90/100**
  (unchanged).
- **Concurrent `register_model`:** unchanged from r6. Silent-leak
  variants observable; panic-unwind leak window remains (R7-M3).
  **90/100** (unchanged).
- **Error-helper consistency / dedup:** new dimension this round —
  the `coded_db` variant-walk consolidation moves this from 75/100
  (r6) to 90/100 at r7. The pipeline-domain error-mapping helpers
  (`audit::coded_sql`, `migrations::coded_db`, `error::coded_sql`,
  `diff::coded_sql`) now share a single variant-walk implementation
  with dedicated contract tests. **+15** on a dimension that did not
  exist in the r6 scoring but exists implicitly under "code quality".

### Comparison to r6 (84)

The +1 is the cleanest possible movement: a single migration-pipeline
commit (`deeefe18`) closes the last instance of a per-module
variant-walk pattern by delegating to a tested shared helper. No
regressions, no new findings of consequence. Two open IMPORTANT
findings (R7-I1, R7-I2) remain in the queue from r2.

**Suggested next attack order (unchanged from r6):**

1. **R7-I2** (F2 close) — `TerminalStatus::ValidationRefused` +
   CHECK constraint extension + flip validate.rs Pending writes to
   immediate-Refused. ~15 LOC. Biggest visibility win for smallest
   change.
2. **R7-M1** — `audit_generation = $2` predicate on
   `set_backfill_running` + 0-rows-affected → `err_reset_externally`
   in exec_begin. ~5 LOC.
3. **R7-I1** (F1 close, partial) — extend the `tracing::warn!` fix
   shape from R5-M7 to the five DDL terminal-transition sites;
   add `owner_session_id` / `last_heartbeat_at` to DDL INSERT.
   ~15 LOC. Sweeper itself is a separate work item (~30-50 LOC).

After (1) + (2) + the `tracing::warn!` part of (3): score floor
moves to ~88-89. Remaining deductions then concentrate in R7-M3
(catch_unwind), R7-M2 (cursor monotonicity), and the lenient
type-split. All are next-stratum refactors.

The r7 delta confirms the r6 projection — the error-shaping rail
hits "single source of truth" status, with the audit-row state
machine still the weakest contributor.
