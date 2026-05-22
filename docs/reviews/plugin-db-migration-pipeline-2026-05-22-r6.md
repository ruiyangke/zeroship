# plugin-db: Migration / DDL Pipeline Correctness Review (R6)

**HEAD:** post-`a272d1af` · **Date:** 2026-05-22 (cycle ~06:00) · **Reviewer:** fresh lens
**Prior rounds:** r1 (72/100) · r2 (81/100) · r3 (81/100) · r4 (82/100) · r5 (83/100)

---

## 0. Scope of this round

Per the dispatch brief, no commit since r5 directly touches the
migration pipeline. The plugin-db delta vs. r5 HEAD (`e399eeea`) is:

| Commit | Module | Migration-pipeline relevance |
|---|---|---|
| `51ced4a0` | `migrations.rs` + `lock_guard.rs` (docs) | `finalise_backfill` now `tracing::warn!` on Err — closes R5-M7 |
| `34d209b5` | `replication_ops.rs` (ConsumerRunningGuard) | replication-side; not pipeline |
| `70921112` | `replication_ops.rs` (atomic try-claim) | replication-side; not pipeline |
| `e399eeea` | `replication_ops.rs` (Drop-clear marker) | replication-side; not pipeline |
| `aa639715` | `wal_consumer.rs` | WalConsumer typed errors; not pipeline |
| `4b2e7046` | `replication_ops.rs` (docs) | docs; not pipeline |
| `cbbc9059` | `error.rs` | dedupe `coded_sql/prefix_message` — call shape unchanged |
| `f7d0961c` | `error.rs` | preamble doc update |
| `f1f06900` | tests | tests only |
| `a272d1af` | `auth/*` | not pipeline |

Net structural change to the migration pipeline since r5: **one** commit
(`51ced4a0`), and it closes the one R5-NEW finding (R5-M7) by wrapping
`finalise_backfill` in `tracing::warn!`. Everything else is replication-
or auth-adjacent.

The four carry-overs to re-audit fresh: **F1** (orphan `Running` DDL),
**F2** (orphan `Pending` validate-refused), **F3 / [I41]** (post-COMMIT
reset clobber, closed at r4), **R5-M1** (`set_backfill_running` no
gen predicate). Plus the dispatch-brief asks: **R5-M8** (cursor
monotonicity, SDK-trusted), **CIC retry budget**, and **concurrent
register_model + migration key-collision**.

---

## 1. Audit dimensions per dispatch brief

### 1.1 F1 / F2 / F3 status check at HEAD

| ID | r5 status | HEAD status | Evidence |
|---|---|---|---|
| **F1** (orphan DDL `Running` audit rows) | OPEN | **OPEN, unchanged** | `apply.rs:163-186` — both terminal branches still `let _ = backend.update_audit_status(...).await;`. `audit.rs:283-316` `write_audit_row` INSERT still does not populate `owner_session_id` / `last_heartbeat_at`. The three CIC retry sites (`postgres.rs:478-486 / 519-528 / 558-567`) likewise unchanged. |
| **F2** (orphan validate-refused `Pending`) | OPEN | **OPEN, unchanged** | `validate.rs:69-87` writes `InitialStatus::Pending` per destructive op. No terminal transition on strict (line 91 returns `Err`) or lenient (apply.rs:203 / 236 skip destructive ops). Status CHECK at `audit.rs:218-220` does not include `validation_refused`. |
| **F3** / [I41] (post-COMMIT reset clobber) | CLOSED at r4 | **CLOSED, verified** | `migrations.rs:594-619` — `update_backfill_progress` inside the same tx as data UPDATEs, AFTER `lock_audit_row_for_update`'s FOR UPDATE, BEFORE COMMIT (line 616). Walked end-to-end below. |
| **R5-M1** (`set_backfill_running` no gen predicate) | OPEN at r5 | **OPEN, unchanged** | `migrations.rs:302-305` still issues `set_backfill_running(&client, app_id, row.id)` without passing `row.audit_generation`. `audit.rs:525-545` `set_backfill_running` UPDATEs `WHERE id = $1::bigint` — no `audit_generation` predicate. |
| **R5-M7** (backfill `finalise_backfill` silent Err) | NEW r5 | **CLOSED at HEAD** | `migrations.rs:644-657` — `if let Err(e) = backend.finalise_backfill(...).await { tracing::warn!(...); }`. The CIC trio (postgres.rs:478-486 / 519-528 / 558-567) stays on `let _ =`; this is now the F1 family without the backfill terminal site. |

### 1.2 R5-M7 closure walk-through

The new finalise-warn site, `migrations.rs:644-657`:

```rust
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

This is the F1-family fix shape r5 prescribed (R5-I1 step 2). The
row state is still permitted to drift to `running`-with-COMMITed-data
on a transient pool error during the terminal transition, but
operators now see a `tracing::warn!` line with `app_id`, `audit_id`,
and the underlying error code. The backfill audit row carries
`owner_session_id` + `last_heartbeat_at` (audit.rs:533-534, 568-569),
so a future heartbeat-driven sweeper can still reap.

The post-warn flow (lines 659-664) is identical to pre-r6:

1. `release_advisory_lock(zs_mig:<app>, name)` (line 660).
2. `drop(client)` — backend session ends, all session-scoped locks
   release server-side regardless of the explicit unlock.
3. `clear_mig_lock` clears the per-isolate slot.

No change to the lock-lifecycle invariants; the only delta is
observable Err on the terminal write. **R5-M7 closed.**

### 1.3 F1 — orphan DDL `Running` (still open)

Five `let _ = update_audit_status(...).await` sites remain:

1. `apply.rs:163-170` — DDL **Applied** transition.
2. `apply.rs:178-186` — DDL **Failed** transition.
3. `postgres.rs:478-486` — CIC `invalid_index_landed` Failed.
4. `postgres.rs:519-528` — CIC `data_violation` Failed.
5. `postgres.rs:558-567` — CIC `transient_retry` / `non_transient_failure` Failed.

R5-M7's fix would generalise here in 5 LOC each; none of these
shipped this round.

**The DDL INSERT asymmetry** is also still open: backfill rows
populate `owner_session_id` and `last_heartbeat_at` at
audit.rs:533-534/568-569; the DDL INSERT at audit.rs:297-308 does
not. So even if a sweeper landed, the DDL rail would be invisible
to it without an additional INSERT change.

### 1.4 F2 — orphan `Pending` validate-refused rows (still open)

`validate.rs:66-95`:

- Destructive ops + `strictness != "off"` → write
  `InitialStatus::Pending` per op via `backend.write_audit_row`.
- Strict path (line 90-92) returns `Err(envelope)` → pipeline
  aborts → rows stay Pending forever.
- Lenient path falls through (line 93-94) → apply skips destructive
  ops at apply.rs:203 and 236 → `update_audit_status` never reached
  → rows stay Pending forever.

The status CHECK at audit.rs:218-220:

```
CONSTRAINT __zeroship_migrations_status_chk CHECK (
  status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back')
)
```

No `validation_refused` — so even if validate.rs wanted to emit a
terminal row, it couldn't write that status without bumping the
CHECK first.

**Same fix shape as r5 (R5-I2):** add `TerminalStatus::ValidationRefused`,
extend the CHECK constraint, write Running + immediate
`update_audit_status(ValidationRefused, …)`. ~15 LOC.

### 1.5 F3 / [I41] — post-COMMIT reset clobber (verified closed)

Re-walked `exec_commit_batch` at HEAD (`migrations.rs:447-668`):

```
take_lock_client                                  (line 471)
client_exec("BEGIN")                              (line 482)
lock_audit_row_for_update(audit_id) FOR UPDATE    (line 497)
  if row.status == "cancelled"  → rollback_and_return; err_cancelled_mid_run
  if row.audit_generation != start_generation  → rollback_and_return; err_reset_externally
for upd in updates_arr:
  validate shape / build UPDATE / client_exec     (line 578)
if !dry_run:
  update_backfill_progress(audit_id, next_cursor,  (line 595)  ← INSIDE the tx, under row lock
                            dlp, processed_total)
client_exec(COMMIT|ROLLBACK)                       (line 616)
if is_done:
  finalise_backfill (`tracing::warn!` on Err)      (line 644-657)  ← R5-M7 close
  release_advisory_lock + drop(client) + clear_mig_lock
```

Row lock from `lock_audit_row_for_update` survives until COMMIT,
serialising any concurrent `migrations.reset` against the cursor
write. The Gap-X generation-mismatch check (line 515) is the only
escape — a reset that beats us to the FOR UPDATE flips
`audit_generation`, and we surface `err_reset_externally` before
mutating data. **[I41] closure verified.**

One residual observation (unchanged from r5): `set_backfill_running`
at `exec_begin` (migrations.rs:302-305) reads the row's
`audit_generation` into `row.audit_generation` then writes
`status='running'` without re-checking the generation in the UPDATE
WHERE clause. This is R5-M1 below; cosmetic per r5 — the next
commit_batch's FOR-UPDATE + generation check catches the drift and
surfaces `err_reset_externally` to the SDK. The data isn't lost.

### 1.6 R5-M8 — cursor monotonicity (SDK-trusted; still latent)

`update_backfill_progress` at `audit.rs:707-736`:

```sql
UPDATE "{app_id}"."__zeroship_migrations"
    SET validate_cursor = $2::bigint,
        dead_letter_pks = $3::jsonb,
        details = jsonb_set(COALESCE(details, '{}'::jsonb), '{processed}', to_jsonb($4::bigint)),
        last_heartbeat_at = NOW(),
        updated_at = NOW()
    WHERE id = $1::bigint
```

No `validate_cursor < $2::bigint` predicate. A buggy SDK or retry
loop that re-issues commit_batch with a stale `nextCursor` (smaller
than the current `validate_cursor`) would silently rewind the
cursor. Per r5: this is *risk-of-reprocessing*, not data loss
(per-row migrations are idempotent by B1 contract). Still
SDK-trusted server-side. **Unchanged from r5.**

Defensive fix: add `AND (validate_cursor IS NULL OR validate_cursor
< $2::bigint)` to the WHERE; surface `migration_cursor_rewind` coded
error on 0 rows affected so the SDK can re-snapshot. ~4 LOC.

### 1.7 CIC retry budget — re-walked

`create_index_with_recovery_audited` at `postgres.rs:385-600`.

`MAX_RETRIES = 3` → 4 iterations (attempts 0..=3). Per attempt:

1. Issue `CREATE INDEX CONCURRENTLY` against the pool (line 455).
2. **Ok path:**
   - `SELECT indisvalid` on the index oid (line 459-466).
   - Valid → return Ok(()). ✓
   - Invalid → write `index_retry` Running audit row (line 476-477),
     best-effort transition to Failed (line 478-486 — `let _ =`),
     `DROP INDEX CONCURRENTLY IF EXISTS` (line 487), exhaustion
     check (line 488) → `SchemaRefused { code: "cic_failed" }` or
     loop.
3. **Err path — fatal SQLSTATEs (`UNIQUE_VIOLATION`,
   `NOT_NULL_VIOLATION`, `FOREIGN_KEY_VIOLATION`, `CHECK_VIOLATION`)**:
   immediate audit + drop + terminal `unique_violation` envelope.
   No retry. ✓
4. **Err path — transient SQLSTATEs (`T_R_DEADLOCK_DETECTED`,
   `DISK_FULL`, `OUT_OF_MEMORY`)**: audit + drop + loop unless on
   final attempt → terminal `cic_failed` envelope. ✓
5. **Err path — anything else**: audit + drop + immediate terminal
   `cic_failed` envelope. No retry.

**Retry budget is sound.** Four attempts; SQLSTATE-classified;
partial-index cleanup on every failure; deterministic terminal
returns. **Unchanged since r5.**

Fallback at line 593-600 (`DbError::Configuration { code:
"cic_configuration" }`) is the loop-exit-without-terminal arm —
unreachable in practice, kept as a tripwire. **R5-M6 stays as
cosmetic.**

### 1.8 Concurrent register_model + concurrent migration — key collision check

Two independent advisory-lock disciplines coexist:

| Caller | Key1 | Key2 | Mode |
|---|---|---|---|
| `register_model` | `hashtext("zs_reg:<app>")` | `hashtext("register_model")` | **blocking** `pg_advisory_lock` |
| `migrations.exec_begin` | `hashtext("zs_mig:<app>")` | `hashtext(<name>)` | **non-blocking** `pg_try_advisory_lock` |

The two key spaces share Postgres' 64-bit advisory lock space
(`(int4, int4)` pair). For a collision between
`(hashtext("zs_reg:<X>"), hashtext("register_model"))` and
`(hashtext("zs_mig:<Y>"), hashtext(<name>))` to actually block, both
the namespace key AND the tag key must collide simultaneously —
joint probability ~1/2⁶⁴ per (X, Y, name) triple. Effectively
impossible.

More importantly, even if a collision occurred:

- `register_model` would queue behind the migration's lock
  (blocking acquire).
- `migrations.exec_begin` would receive `false` from
  `pg_try_advisory_lock` and surface `err_already_running()` (a
  user-facing coded error the SDK can branch on) — *better* failure
  mode than a hang.

The two locks are **disjoint in key space (different prefixes), use
different acquisition modes (blocking vs. try), and are semantically
distinct** (serialise DDL deploys vs. refuse concurrent backfill
runs on the same (app, name)). **No collision risk in practice.**
Unchanged since r5.

### 1.9 `__zeroship_migrations` table schema — no changes

`ensure_audit_table_exists` at `audit.rs:181-261`. Schema unchanged
from r5: 23 columns including `audit_generation BIGINT NOT NULL
DEFAULT 0` (added idempotently via `ALTER TABLE … ADD COLUMN IF NOT
EXISTS` at audit.rs:234-240). Status CHECK at lines 218-220 still
lacks `validation_refused`. Phase CHECK lines 212-214 still lacks
nothing relevant (`ddl/validation/backfill/audit`).

No new ADRs or migrations added this cycle.

### 1.10 `coded_sql` / `prefix_message` dedupe (cbbc9059)

`cbbc9059` consolidates the `coded_sql` / `prefix_message` helpers
that 5 sites had independently. The pipeline-domain sites that route
Postgres errors through this layer:

- `audit.rs:58-60` — `audit::coded_sql` (the `audit: <ctx>:` prefix).
- `migrations.rs:82-102` — `coded_db` (the `<ctx>: db: ...` prefix
  applied through the typed `DbError`).
- `backend/postgres.rs` — uses `crate::error::coded_sql` at the
  facade layer.
- `diff.rs` — uses the same.

The dedupe collapses these into a single implementation in
`crate::error`. **No behaviour change at any call site;** the
operator-facing prefix string and the SQLSTATE-derived `.code` are
preserved verbatim. Walked one site (`audit.rs:58-60`) to confirm
the wrapper still applies `audit: <ctx>` prefix correctly.

---

## 2. Pipeline diagram at HEAD (r6)

Functionally identical to r5 except the finalise-warn site:

```
register_model_dispatch                  (orchestrator/register_model/mod.rs:63)
        │
        ├─ [fast path] is_model_registered? → resolve(undefined)
        │
        └─ exec_register_model                    (mod.rs:108)
               ▼
          bootstrap                               (bootstrap.rs:78)
            pool.get() → lock_client
            OrchestratorLockGuard::acquire        (lock_guard.rs:117)
              [#[must_use] (808a32af)]
            ├── build_ctx: ensure_app_schema / ensure_audit_table /
            │     next_schema_version / expand declared_indexes
            │   ──Err──► guard.release().await; return Err(e)
            └── Ok → (RegisterContext, OrchestratorLockGuard<'p>)
               ▼
          compute_plan         ──Err──► mod.rs:217 guard.release().await
               │                          [released flag flips AFTER unlock await (bd1e7ce1)]
               │                          [tracing::warn on unlock SQL error (ffb1e101)]
               ▼
          validate
            destructive + strictness != "off" → Pending audit rows (F2 ORPHAN)
            strict   → Err(envelope_json) → wrap as DbError::SchemaRefused
            lenient  → Ok(ApprovedPlan{ ops with destructive RETAINED })  (F2 ORPHAN)
            off      → Ok(ApprovedPlan{ ops with destructive RETAINED })
               ▼
          apply
          ┌─ Pass 1 (under advisory lock — async block apply.rs:201) ──┐
          │  for op in approved.ops:                                    │
          │    skip Destructive; skip AddIndex                          │
          │    check_destructive_invariant(op)? (3ef6a170)              │
          │    write_audit_row(Running)                                 │
          │    pool_exec(op.sql) | create_index_with_recovery (AddIndex)│
          │    update_audit_status(Applied|Failed)  ← `let _ =`  (F1)   │
          └─────────────────────────────────────────────────────────────┘
          guard.release().await           (apply.rs:226 — always)
          pass1?                          (propagate after release)
          ┌─ Pass 2 (unlocked) ─────────────────────────────────────────┐
          │  for op in approved.ops (AddIndex only): run_op             │
          └─────────────────────────────────────────────────────────────┘

Backfill orchestrator (migrations.rs) — R5-M7 closed:
  exec_begin          → acquire_dedicated_client + try_advisory_lock(zs_mig:<app>, name)
                        ensure_audit_table (map_audit_bootstrap_err)
                        find_latest_backfill_row → snapshot start_generation
                        insert_backfill_running OR set_backfill_running ← no gen-check (R5-M1 = R6-M1)
                        park client in ctx.mig_lock
       ▼
  exec_fetch_batch    → peek_latest_backfill_status; SELECT WHERE id > cursor LIMIT n;
                        heartbeat_backfill (best-effort)
       ▼
  exec_commit_batch   → BEGIN
                        lock_audit_row_for_update         ← FOR UPDATE row lock
                        if status='cancelled'     → rollback_and_return; err_cancelled_mid_run
                        if audit_generation drift → rollback_and_return; err_reset_externally
                        for upd: UPDATE schema.table SET … WHERE id = $1
                        update_backfill_progress          ← R4 FIX preserved (no monotonicity check — R6-M2)
                        COMMIT/ROLLBACK
                        if is_done → finalise_backfill (`if let Err = ... tracing::warn!`)  ← R5-M7 CLOSED
                                   + release_advisory_lock + drop(client) + clear_mig_lock
```

---

## 3. R6 findings

### CRITICAL

(none)

### IMPORTANT

**R6-I1 — F1: Orphan `Running` DDL audit rows (carry-forward from R5-I1).**

- **File:** `crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186`;
  `crates/plugin-db/src/audit.rs:283-316`;
  `crates/plugin-db/src/backend/postgres.rs:478-486 / 519-528 / 558-567`
- **Why:** Five `let _ = update_audit_status(...).await;` sites
  silently drop typed `Result`s on terminal DDL audit transitions.
  The DDL `write_audit_row` INSERT does not populate
  `owner_session_id` / `last_heartbeat_at`, so an orphan `Running`
  row from a failed terminal UPDATE is indistinguishable from a
  real in-flight DDL by any operator scan.
- **Why: data loss / stall / cross-app impact:** No data loss; no
  stall (the OrchestratorLockGuard always releases on observable
  Err paths). Operator-state observability rot: per-deploy at least
  one stale `Running` row possible after a transient terminal-
  UPDATE failure. Blocks any future drift-detection / manual-
  approve UX. Compounds with R6-M3 (Drop-path panic — guard releases
  via Drop's sync log only).
- **Fix:**
  1. Add `owner_session_id = pg_backend_pid()::text,
     last_heartbeat_at = NOW()` to the DDL INSERT column list
     (audit.rs:286-308).
  2. Replace each `let _ = update_audit_status(...).await;` with
     `if let Err(e) = ... { tracing::warn!(audit_id = id, error = ?e,
     "audit: terminal-transition update failed; row stays Running
     until sweeper reaps"); }`. Applies to apply.rs:163-170 and
     178-185 plus the three CIC-loop sites
     (postgres.rs:478-486, 519-528, 558-567). The R5-M7 close at
     migrations.rs:644-657 is the model.
  3. Ship a heartbeat-driven sweeper: scan
     `phase='ddl' AND status='running' AND (last_heartbeat_at IS NULL
     OR last_heartbeat_at < NOW() - INTERVAL '5 min')` and demote
     to `failed` with a synthesised error.
- **Verification:** apply.rs:163-170 (Applied `let _ =`);
  apply.rs:178-185 (Failed `let _ =`); audit.rs:286-308 (INSERT
  column list — no session/heartbeat); audit.rs:525-545 (backfill
  `set_backfill_running` *does* stamp pid+heartbeat — DDL is
  asymmetric).

**R6-I2 — F2: Orphan `Pending` validate-refused audit rows (carry-forward from R5-I2).**

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:66-95`;
  `crates/plugin-db/src/audit.rs:218-220` (status CHECK)
- **Why:** validate.rs writes a `Pending` audit row per destructive
  op whenever `ctx.strictness != "off"`. Strict path returns Err →
  pipeline aborts → rows stay Pending. Lenient path returns Ok with
  destructive ops still in `plan.ops`; apply skips them at
  apply.rs:203 and 236 so `update_audit_status` is never called —
  rows stay Pending. The status CHECK constraint does not include
  `validation_refused`, so even if the pipeline wanted to write a
  terminal "refused" status it would hit SQLSTATE 23514.
- **Why: data loss / stall / cross-app impact:** No data loss; no
  stall. Pure observability pollution — the audit table grows
  unboundedly with phantom Pending rows on every strict refusal /
  lenient skip.
- **Fix:**
  1. Extend `__zeroship_migrations_status_chk` (audit.rs:218-220) to
     include `validation_refused`.
  2. Add `TerminalStatus::ValidationRefused` (audit.rs:138-155) with
     `as_sql() = "validation_refused"`.
  3. Change validate.rs:67-88 to write Running and immediately
     `update_audit_status(ValidationRefused, …)` — mirrors the
     apply-layer state-machine pattern; no new `InitialStatus`
     variant needed.
- **Verification:** validate.rs:69-87 (Pending write site);
  validate.rs:90-92 (strict returns Err with no terminal transition);
  validate.rs:93-94 (lenient falls through with destructive ops
  retained); apply.rs:203 + 236 (destructive skip path — never
  reaches `update_audit_status`); audit.rs:218-220 (status CHECK
  constraint — `validation_refused` not present).

### MINOR

**R6-M1 — `set_backfill_running` lacks generation predicate (carry-forward R5-M1).**

- **File:** `crates/plugin-db/src/migrations.rs:302-305`;
  `crates/plugin-db/src/audit.rs:525-545`
- **Why:** `exec_begin` reads the snapshot then calls
  `set_backfill_running(&client, app_id, row.id)` without passing
  `row.audit_generation`. An operator `migrations.reset` between the
  SELECT and the UPDATE flips the row's `audit_generation` from
  `g0` to `g0+1`. Our `set_backfill_running` runs `UPDATE … status =
  'running' WHERE id = $1::bigint` and overwrites the reset's
  `'pending'`. The reset isn't *lost* — the next commit_batch
  detects the generation mismatch and surfaces
  `err_reset_externally` — but the row briefly shows
  `status='running'` to operator-side audit scans.
- **Operator impact:** transient UI ghost (`status='running'` for
  ~one batch interval). Self-heals at next `commit_batch`.
  Cosmetic.
- **Fix:** add `AND audit_generation = $2::bigint` to
  `set_backfill_running`'s WHERE (audit.rs:530-538) and pass the
  freshly-read `row.audit_generation` from migrations.rs:303; on
  `0 rows affected`, return `err_reset_externally()` from
  `exec_begin` directly so the SDK mints a fresh wrapper. ~5 LOC.
- **Verification:** migrations.rs:288-306 (snapshot-then-set is
  non-transactional, no gen-check passed); audit.rs:530-543 (UPDATE
  keyed only by id).

**R6-M2 — `update_backfill_progress` cursor monotonicity SDK-trusted (carry-forward R5-M8).**

- **File:** `crates/plugin-db/src/audit.rs:707-736`
- **Why:** `update_backfill_progress` runs `UPDATE … SET
  validate_cursor = $2::bigint … WHERE id = $1::bigint` with no
  predicate that `$2 > previous validate_cursor`. A buggy SDK or
  rolled-back-batch-then-retry path that re-issues commit_batch with
  a stale `nextCursor` would silently rewind the cursor.
- **Why: data loss / stall / cross-app impact:** Narrow — the
  cursor advance is what the SDK uses to skip already-processed
  rows. A rewound cursor causes *reprocessing*, not data loss; the
  per-row migration must be idempotent (already a B1 contract). So
  this is risk-of-redundant-work, not risk-of-skip.
- **Fix:** add `AND (validate_cursor IS NULL OR validate_cursor <
  $2::bigint)` to the WHERE; on 0 rows affected, surface
  `migration_cursor_rewind` coded error so the SDK can re-snapshot.
  ~4 LOC.
- **Verification:** audit.rs:715-734 (UPDATE keyed only by id).
- **Severity:** latent / defensive. SDK is trusted today; would need
  to be a bug in `@zeroship/migrations` or an SDK retry-with-stale-
  state to trigger.

**R6-M3 — `OrchestratorLockGuard::Drop` panic path still mitigated only by logs (carry-forward R5-M3).**

- **File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:212-240`
- **Round-r6 movement:** none. The r5 trio (`808a32af`, `bd1e7ce1`,
  `ffb1e101`) is unchanged at HEAD.
- **Residual:** a panic during pipeline execution → Drop logs →
  pooled client returns to pool with session-scoped lock held →
  subsequent register_model callers stall until backend session
  ends. The lock auto-releases on session close (typically pool
  recycle, tens of seconds to minutes). The `AssertUnwindSafe` +
  `catch_unwind` at the pipeline boundary is still unshipped.
- **Why: cross-app impact:** an unwind that escapes Drop's
  best-effort path stalls *every concurrent register_model caller
  for that app* until the pool recycles the connection.
- **Fix:** wrap the inner `run_pipeline` body in `catch_unwind` at
  the dispatch boundary; on panic, explicitly run the unlock SQL
  before re-raising. Documented but unshipped since r4.

**R6-M4 — F1 retry-sites at `create_index_with_recovery_audited` use string-interpolated `pg_index` validity check (carry-forward R5-M4).**

- **File:** `crates/plugin-db/src/backend/postgres.rs:459-466`
- **Why:** `SELECT indisvalid FROM pg_index WHERE indexrelid = '{}'::regclass`
  interpolates `qualified_idx` via `replace('\'', '\'\'')`. Index
  names come from `query::build_*_indexes` (caller-controlled, but
  validated upstream); still belt-and-braces.
- **Fix:** `query_text_params("… WHERE indexrelid = $1::regclass",
  &[qualified_idx.as_str()])`. ~3 LOC.

**R6-M5 — `exec_cancel` operator path strands the in-flight worker's lock client (carry-forward R5-M5).**

- **File:** `crates/plugin-db/src/v8_classes/migration.rs:84-131`;
  `crates/plugin-db/src/migrations.rs:714-742`
- **Why:** `exec_cancel` writes `status='cancelled'` to the audit row
  via the pool (no lock) and returns. The in-flight worker's parked
  `mig_lock` client is not touched. The worker's next `fetchBatch`
  reads the cancelled status and aborts; the lock releases when the
  worker calls `clear_mig_lock` or when the isolate tears down.
  Documented at `v8_classes/migration.rs` Drop comments.
- **Why: stall:** the cancelled migration's `(name, collection)` is
  wedged on that isolate for up to one batch interval (worker's
  fetchBatch polling cadence) before the cancel observation
  triggers the abort + release. No cross-app impact (lock is keyed
  on `(zs_mig:<app>, <name>)`).
- **Severity:** not actively a worsening. Same posture as r5.

**R6-M6 — CIC retry-exhaustion fallback variant divergence (carry-forward R5-M6).**

- **File:** `crates/plugin-db/src/backend/postgres.rs:593-600`
- **Why:** Loop-exit-without-terminal returns
  `DbError::Configuration { code: "cic_configuration" }` while every
  other terminal arm returns `SchemaRefused { code: "cic_failed" }`.
  Cosmetic; defence-in-depth tripwire. Unreachable in current code.
- **Fix:** harmonise the fallback code to `cic_failed` for SDK
  consistency, or alternatively keep the divergent code with a
  documented "invariant breach" tag (the current state). ~3 LOC.

**R6-M7 — Apply Pass-2 idempotency assumption uncovered for `AddIndex` retry (informational).**

- **File:** `crates/plugin-db/src/orchestrator/register_model/apply.rs:230-240`
- **Observation:** Pass-2 runs CIC ops after the advisory lock is
  released. If two orchestrators race past their Pass-1 release on
  the same app and reach Pass-2 concurrently, both attempt
  `CREATE INDEX CONCURRENTLY IF NOT EXISTS`. Postgres' CIC takes a
  share-update-exclusive lock on the underlying table; concurrent
  CIC on the same table on the same name returns immediately on the
  IF NOT EXISTS branch (winner builds, loser observes existence and
  returns `Ok`). **No correctness gap.** Worth noting because the
  comment at apply.rs:9-14 says "two orchestrators racing on CIC is
  safe (IF NOT EXISTS)" — confirmed in r6 against the actual SQL
  emission paths in `query::build_create_indexes`. Informational.

---

## 4. Stage invariants at HEAD (r6 update)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Guard held (#[must_use] (808a32af)); schema + audit table exist; `schema_version` monotonic | Err → `guard.release().await; return Err` |
| **plan** | Lock held | `Vec<DiffOp>` classified ([I28] diff.rs returns DbError on Err — `compute_diff` is pure) | Err → caller releases (mod.rs:217) |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows NOT terminalised** (R6-I2 / F2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows Running→Applied/Failed; **terminal UPDATE errors silently discarded** (R6-I1 / F1); `check_destructive_invariant` traps misclassified Drops | Err → release (line 226), then `pass1?` propagates |
| **apply pass 2** | Lock released | CIC ops run idempotently (`IF NOT EXISTS`); retry loop with `pg_index` indisvalid check + audited retry log (4 attempts; deterministic SQLSTATE classification) | terminal → `SchemaRefused { code: "cic_failed" }`; fallback → `cic_configuration` (R6-M6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`set_backfill_running` lacks gen predicate** (R6-M1); `insert_backfill_running` raises DbError::Internal on empty RETURNING via `first_row_or_internal` (eda96ead) | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held | BEGIN; row FOR UPDATE inside tx; generation check inside tx; per-row UPDATEs; **`update_backfill_progress` inside tx under row lock** (R4 [I41] fix preserved); COMMIT/ROLLBACK; is_done → `finalise_backfill` (**`tracing::warn!` on Err — R5-M7 closed at HEAD**) + release + drop | dry-run → ROLLBACK; partial-failure → `rollback_and_return` |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release in-flight worker's mig advisory lock** (R6-M5) | benign for audit row; isolate wedged until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — closed at r4 by [I41] |
| **OrchestratorLockGuard::release** | Guard live | `pg_advisory_unlock` issued (best-effort, but **logs `tracing::warn!` on SQL error** (ffb1e101)); released flag set AFTER unlock await (bd1e7ce1); returns unlocked PooledClient | SQL errors logged not swallowed silently |
| **OrchestratorLockGuard::drop** (panic / forgotten release) | n/a | `tracing::error!` with "leak:" prefix + diagnostic checklist (808a32af); PooledClient returns with lock held; auto-release on backend session close | Documented catastrophic fallback (R6-M3); reduced silent variant |

---

## 5. Audit-row terminal-transition reliability (r6 re-tally)

Sites that discard a typed `Result` from a state-transition write
(the F1 family):

1. **apply.rs:163-170** — DDL Applied transition (F1, R6-I1).
2. **apply.rs:178-186** — DDL Failed transition (F1, R6-I1).
3. **postgres.rs:478-486** — CIC `invalid_index_landed` Failed (F1 family).
4. **postgres.rs:519-528** — CIC `data_violation` Failed (F1 family).
5. **postgres.rs:558-567** — CIC `transient_retry` / `non_transient_failure` Failed (F1 family).

**Site count dropped from 6 to 5 in r6** (closed: backfill
`finalise_backfill` at migrations.rs:644-657 — R5-M7).

The R6-I1 fix shape — `if let Err(e) = ... { tracing::warn!(...); }`
— applies uniformly. The R5-M7 close at migrations.rs is the
in-tree model.

OrchestratorLockGuard does not protect these — the audit writes
either go through the pool (DDL Apply, CIC) or through the dedicated
mig_lock client (backfill `finalise_backfill`); the guard's
release-on-Err invariant is orthogonal to per-write error handling.

---

## 6. Concurrent register_model + concurrent migration — verified

The two lock disciplines remain disjoint in key space, in
acquisition mode, and in semantic intent. No commits since r5
touched the lock-key construction in either path:

- `bootstrap.rs:59-63` — register_model uses `lock_key(app_id) =
  "zs_reg:<app>"` + `LOCK_TAG = "register_model"`.
- `migrations.rs:269` — `let lock_key = format!("zs_mig:{app_id}");`
  + `name` as tag.

Hashtext-space collision: ~1/2⁶⁴ per (app, migration_name) pair.
Even on collision, register_model blocks; migration sees
`pg_try_advisory_lock = false` → `err_already_running()`. No
cross-discipline interference.

**Unchanged from r5: no collision risk in practice.**

---

## 7. Score

**84 / 100** (+1 vs r5: **83**)

### Movement vs r5

- **+1 for `51ced4a0` closing R5-M7.** `finalise_backfill` Err is
  now `tracing::warn!`'d; one site dropped from the F1 family
  (6 → 5). The backfill terminal rail now matches the F1-fix shape
  R5-I1 prescribed. Improves the audit-row state-machine consistency
  dimension that has been the weakest contributor (65/100 at r5).
- **+0 for `cbbc9059` (error-helper dedupe).** Mechanically safe;
  no call-site behaviour change.
- **+0 for `34d209b5` / `70921112` / `e399eeea` / `aa639715`.**
  Replication / WAL-consumer-side; not on the migration-pipeline
  rail.
- **+0 for `f7d0961c` / `4b2e7046` / `f1f06900`.** Doc / test only.
- **F1 (R6-I1), F2 (R6-I2), R6-M1, R6-M2 unchanged.** Same dispatch
  recommendation as r5: ship F2 (smallest LOC), then R6-M1 (~5
  LOC), then F1 + sweeper (~30-50 LOC).

### Score components

- **Strict-deploy correctness:** strong; plan/validate/apply
  boundaries crisp; guard `#[must_use]` so accidental drops warn
  at compile time. **91/100** (unchanged).
- **Lenient/off-deploy correctness:** unchanged — split invariant
  remains. **75/100**.
- **Audit-row state machine consistency:** R5-M7 close lifts this
  by one notch (backfill terminal now observable). Two orphan
  classes (F1, F2) remain; five `let _ =` DDL sites unchanged.
  **68/100** (was 65).
- **Backfill orchestrator:** in-tx progress UPDATE sound (R4 [I41]
  preserved); row lock semantics correct; cross-batch generation
  check intact. R6-M1 residual; R6-M2 SDK-trusted cursor monotonicity
  latent. Terminal transition now warn-on-err. **82/100** (was 80).
- **CIC recovery loop:** robust; deterministic SQLSTATE
  classification; bounded retry; partial-index cleanup. **90/100**
  (unchanged).
- **Concurrent `register_model`:** unchanged from r5. Silent-leak
  variants are observable; panic-unwind leak window remains
  (R6-M3). **90/100** (unchanged).

### Comparison to r5 (83)

The +1 is the cleanest possible movement: a single commit
(`51ced4a0`) closes a NEW finding (R5-M7) from the previous round
by applying the same fix shape R5-I1 prescribed. No regressions, no
new findings of consequence (R6-M7 is informational confirming an
existing comment). Two open IMPORTANT findings (R6-I1, R6-I2) remain
in the queue from r2.

**Suggested next attack order (unchanged from r5):**

1. **R6-I2** (F2 close) — `TerminalStatus::ValidationRefused` +
   CHECK constraint extension + flip validate.rs Pending writes to
   immediate-Refused. ~15 LOC. Biggest visibility win for smallest
   change.
2. **R6-M1** (R5-M1 close) — `audit_generation = $2` predicate on
   `set_backfill_running` + 0-rows-affected → `err_reset_externally`
   in exec_begin. ~5 LOC.
3. **R6-I1** (F1 close, partial) — extend the `tracing::warn!` fix
   shape from R5-M7 to the five DDL terminal-transition sites;
   add `owner_session_id` / `last_heartbeat_at` to DDL INSERT.
   ~15 LOC. Sweeper itself is a separate work item (~30-50 LOC).

After (1) + (2) + the `tracing::warn!` part of (3): score floor
moves to ~87-88. Remaining deductions then concentrate in R6-M3
(catch_unwind), R6-M2 (cursor monotonicity), and the lenient
type-split (R5-M2 carry-over). All are next-stratum refactors.

The r6 delta confirms the r5 projection — incremental closure of the
F1 family on the backfill rail, with the DDL rail still pending.
