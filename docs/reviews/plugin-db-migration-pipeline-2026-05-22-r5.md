# plugin-db: Migration / DDL Pipeline Correctness Review (R5)

**HEAD:** post-`e399eeea` · **Date:** 2026-05-22 (cycle 04:35) · **Reviewer:** fresh lens
**Prior rounds:** r1 (72/100) · r2 (81/100) · r3 (81/100) · r4 (82/100, cycle 04:00)

---

## 0. Scope of this round

Per the dispatch brief, only one structural commit could plausibly touch
the migration pipeline since r4 — the [I28] sweep at `0049d9be`. The
visible plugin-db delta vs. r4 HEAD (`e24ac662`):

| Commit | Module | Migration-pipeline relevance |
|---|---|---|
| `0049d9be` | `diff.rs` (×3 sites) + auth/replication | error-type sweep; verify no semantic drift |
| `91830cca` | `replication.rs` | replication-only — not pipeline |
| `bd1e7ce1` | `lock_guard.rs` | `release()` flips `released = true` AFTER unlock await — [I42] |
| `4cbe9fa1` | `v8_classes/subscription.rs` | not pipeline |
| `07205e54` | `validate.rs` (doc) | doc-lie fix on `SchemaRefused` (~3 lines, no semantics) |
| `808a32af` | `lock_guard.rs` | `#[must_use]` + louder Drop log |
| `ffb1e101` | `lock_guard.rs` | `tracing::warn!` on unlock-SQL errors |
| `c0590506` | `v8_classes/replication.rs` | scope watchdog — replication, not pipeline |
| `eda96ead` | `audit.rs` (×2) + error.rs + replication.rs | extract `first_row_or_internal` helper |
| `e399eeea` | `replication_ops.rs` | not pipeline |

Net structural change to the migration pipeline since r4:

1. **`diff.rs`** — three error-rail sites converted from `String` to
   `DbError` via a new `coded_sql` helper. Output shape and classifier
   semantics unchanged.
2. **`lock_guard.rs`** — three hardening commits. The RAII contract
   is now `#[must_use]` (compile-time warning floor), the unlock-SQL
   error is logged not swallowed, and the released-flag flip is
   deferred to AFTER the unlock await so a cancellation between flip
   and unlock cannot silently leak the lock with no Drop log.
3. **`audit.rs`** — two empty-RETURNING sites converted to use the
   `first_row_or_internal()` helper. `insert_backfill_running`'s
   `DbError::Internal` contract is preserved verbatim; the error
   message body's punctuation shifted (`"audit: <op> returned no row"`
   → `"audit: <op>: returned no row"`).

The four bugs r4 carried — **F1** (orphan DDL `Running`), **F2**
(orphan validate-refused `Pending`), **F3** (post-COMMIT reset clobber
race, closed at r4), and **R4-M1** (`set_backfill_running` missing
generation predicate) — are all unchanged at HEAD. No commit since
r4 touched `migrations.rs` or `orchestrator/register_model/`.

---

## 1. Audit dimensions per dispatch brief

### 1.1 F1 / F2 / F3 status check

| ID | r4 status | HEAD status | Evidence |
|---|---|---|---|
| **F1** (orphan DDL `Running` audit rows) | OPEN | **OPEN, unmoved** | `apply.rs:163-186` — `let _ = backend.update_audit_status(...).await;` on both Applied and Failed transitions. DDL `write_audit_row` INSERT still does not populate `owner_session_id` / `last_heartbeat_at` (`audit.rs:294-326`). |
| **F2** (orphan validate-refused `Pending`) | OPEN | **OPEN, unmoved** | `validate.rs:69-87` writes `InitialStatus::Pending` per destructive op. No terminal transition on strict (line 91 returns `Err`) or lenient (apply.rs:203, 237 skip destructive ops). Status CHECK constraint at `audit.rs:229-231` still does not include `validation_refused`. |
| **F3** / [I41] (post-COMMIT reset clobber) | CLOSED at r4 | **CLOSED, verified** | `migrations.rs:594-619` — `update_backfill_progress` is now line 595, COMMIT is line 616. Row lock from `lock_audit_row_for_update` (line 497) is held until COMMIT/ROLLBACK. End-to-end walked under §1.3 below. |
| **R4-M1** (`set_backfill_running` no gen predicate) | NEW in r4 | **OPEN, unmoved** | `migrations.rs:302-305` still issues `set_backfill_running(&client, app_id, row.id)` with no generation argument; `audit.rs:541-561` `set_backfill_running` UPDATEs `WHERE id = $1::bigint` — no `audit_generation` predicate. |

### 1.2 diff.rs typed-error sweep — semantics unchanged

The [I28] commit `0049d9be` touched three call-sites in `diff.rs`:

```
read_live_schema           Result<LiveSchema, String>  → Result<LiveSchema, DbError>
estimate_row_count         Result<i64, String>         → Result<i64, DbError>
count_violating_not_null   Result<(i64, Vec<i64>), String> → same Ok shape, DbError on Err
```

Each `map_err` migrated from `|e| format!("diff: <ctx> failed: {e}")`
to `|e| coded_sql("<ctx> failed", e)`. The new `coded_sql` (diff.rs:37-53)
mirrors the audit-layer / migrations-layer pattern: it routes the
Postgres error through `DbError::from`, then prepends `"diff: <ctx>: "`
to the message body of the seven SQLSTATE-coded variants
(`UniqueViolation`, `FkViolation`, `NotNullViolation`, `CheckViolation`,
`Serialization`, `LockContention`, `Transient`, `Internal`).

**`compute_diff` itself** (`diff.rs:437-703`) is a **pure function over
`LiveSchema` + declared schema** — no fallible I/O, no Result return,
no error type change. The [I28] sweep cannot have altered its output
shape. The 14 unit tests in `diff.rs::tests` continue to pin:

- `nullable_add_is_additive` / `required_add_on_empty_table_is_additive`
- `required_no_default_on_non_empty_is_destructive`
- `required_with_default_on_non_empty_is_compatible`
- `drop_column_is_destructive`
- `create_table_emitted_when_live_empty`
- `b2_add_fk_to_existing_column_is_compatible` /
  `b2_existing_fk_no_change_is_no_op` /
  `b2_policy_change_emits_drop_then_add`
- `c2_new_variant_field_classifies_as_additive` /
  `c2_removed_variant_field_classifies_as_destructive`

**`count_violating_not_null`** retains its existing call-site contract
in the validate pass (currently unwired — A2 line 153 budget loop is
"deferred to follow-up PR" per validate.rs:97-106). The function's
Ok shape `(i64, Vec<i64>)` is unchanged; the Err arm now carries a
typed `DbError` with SQLSTATE-derived `.code` instead of a stringly
"diff: count_violating_not_null …" message. **Operator-facing
context is preserved** ("diff: <ctx>:" prefix); SDK retry semantics
improve (transient SQLSTATE now classifies as `.code = "transient"`
instead of being swallowed by an outer wrapper).

**Verdict on diff.rs / I28:** the sweep is mechanically safe.
Classifier output, validate-budget hooks, and operator-context shape
are all unchanged. Net win: typed-error rail is now end-to-end
through `diff::*` (previously the lone breaks in the otherwise typed
register-model orchestrator).

### 1.3 OrchestratorLockGuard ↔ migration backfill — distinct lock keys, distinct mechanisms

Two **independent** advisory-lock disciplines coexist:

| Caller | Key | Acquire | Release | Guard? |
|---|---|---|---|---|
| `register_model` (DDL orchestrator) | `(hashtext("zs_reg:<app>"), hashtext("register_model"))` | `pg_advisory_lock(...)` — blocking | `OrchestratorLockGuard::release().await` (always pre-CIC + on Err) | **Yes** (lock_guard.rs:51-220) |
| `migrations.exec_begin` (backfill) | `(hashtext("zs_mig:<app>"), hashtext(<name>))` | `pg_try_advisory_lock(...)` — non-blocking | `release_advisory_lock` on the dedicated `Client` (mig_lock context slot), plus terminal `drop(client)` which closes the backend session and auto-releases | **No** — uses bespoke `MigrationLock` context slot |

The dispatch brief raised the question: *should `exec_begin` also use
an `OrchestratorLockGuard`-style RAII guard?* My answer: **the
current shape is the right one for now**. The reasoning:

1. **The lock outlives the function.** `exec_begin` parks the
   advisory-lock client in the per-isolate context's `mig_lock` slot
   (`migrations.rs:337-350`) so subsequent `fetchBatch` / `commitBatch`
   calls — issued from JS across many event-loop turns — can issue
   SQL on the same backend session that holds the lock. The DDL
   guard, in contrast, exists only for the duration of one
   `run_pipeline` invocation. An RAII guard whose release point is
   "the next JS call that decides to finalise" doesn't fit the RAII
   pattern; the lifecycle is **already** state-machine-driven, not
   scope-driven.
2. **The `MigrationLock` context slot is the guard.** It is the
   single place that owns the client + lock-state pair; `clear_mig_lock`
   (`migrations.rs:647, 755`) is the deterministic release point
   invoked from `commit_batch(isDone=true)`, `Migration::Drop` (the V8
   finalizer at `v8_classes/migration.rs:84-131`), and the test
   helper `clear_migration_lock_for_tests` (`lib.rs:236-248`). The
   slot pattern is already RAII-equivalent across the three exit
   surfaces; introducing a second guard would not subsume any
   of them.
3. **`try_advisory_lock` vs. `advisory_lock`.** The migration path uses
   non-blocking try-lock and returns `err_already_running()` on
   contention. The DDL path uses blocking lock so the second caller
   waits in PG's lock queue. These are semantically different
   coordination policies — concurrent migration starts on the same
   `(app, name)` MUST surface as "another worker is running this"
   rather than queueing, because the second worker would be holding
   its own audit-row state stale. The lock-guard abstraction is
   currently `acquire_advisory_lock`-only (the blocking variant);
   wrapping the try-variant would require a separate
   `try_acquire_advisory_lock` constructor with a different Ok/Err
   shape.
4. **The escape hatches that *do* leak the lock are already covered
   by `clear_mig_lock` / `migration::Drop`.** R4-M5 (carry-forward
   F8) documents one remaining gap: `exec_cancel` (operator-initiated
   from a different isolate / connection) walks the audit row to
   `cancelled` but does NOT touch the in-flight worker's parked
   `mig_lock` client. That's a state-machine bug, not a guard bug —
   the in-flight worker's next `fetchBatch` reads the cancelled state
   and aborts; the lock releases when the worker calls `clear_mig_lock`
   or when the isolate tears down. A guard wouldn't fix it.

**Verdict on §1.3:** the asymmetric pattern is correct. The DDL path
uses RAII because the lock is scope-bound; the migration path uses a
context slot because the lock is state-machine-bound. The recent
`OrchestratorLockGuard` hardening (must_use, deferred-flag, unlock
warn) is *additive* to the DDL path; the migration path didn't need
those because it never had the bug class (`pg_advisory_unlock` errors
on the mig_lock client *are* logged via `coded_db` since the typed
sweep, and the lock-release sequence in `migrations.rs:643-647` is
sequenced as unlock → drop, so the deferred-flag invariant is
structurally satisfied — there is no boolean to flip).

### 1.4 Backfill batch boundary — cursor advance + COMMIT pairing

Walked again at HEAD as a fresh-lens check. The `exec_commit_batch`
body (`migrations.rs:447-653`):

```
take_lock_client                                  (line 471)
client_exec("BEGIN")                              (line 482) ── Err → return_lock_client; coded_db("BEGIN")
lock_audit_row_for_update(audit_id) FOR UPDATE    (line 497) ── Err → rollback_and_return; coded_db("audit lock")
  if row.status == "cancelled"                              → rollback_and_return; err_cancelled_mid_run
  if row.audit_generation != start_generation               → rollback_and_return; err_reset_externally
for upd in updates_arr:
  validate object shape / id / set                          → rollback_and_return on shape err
  if set is empty/null → continue
  build UPDATE schema.table SET … WHERE id = $1   (line 573)
  client_exec(sql, params)                        (line 578) ── Err → rollback_and_return; coded_db("migration row UPDATE …")
if !dry_run:
  update_backfill_progress(audit_id, next_cursor,
                            dlp, processed_total) (line 595) ── Err → rollback_and_return; coded_db("audit row update")
                                                  ──── BEFORE COMMIT — R4-fix line ─────
client_exec(COMMIT | ROLLBACK)                    (line 616) ── Err → return_lock_client; coded_db("migration <stmt>")
if is_done:
  finalise_backfill(audit_id, terminal, error)              (best-effort `let _ =`)
  release_advisory_lock(zs_mig:<app>, name)                  (line 644)
  drop(client)                                               (line 646; session ends → lock auto-releases anyway)
  clear_mig_lock
  return { committed, done: true }
return_lock_client(client)                        (line 651)
return { committed, done: false }
```

**Checkpoints:**

- Cursor advance (`update_backfill_progress`) happens **inside** the
  same transaction as the data UPDATEs **and** under the audit row's
  `FOR UPDATE` lock acquired at line 497. ✓ (matches r4 verification
  of [I41]).
- `rollback_and_return` (lines 476-479) issues `ROLLBACK` on the
  client BEFORE returning it to the slot — partial-batch state can
  never survive past a batch failure. ✓
- COMMIT failure path (lines 616-619) — PG aborts the transaction
  server-side, so the explicit `return_lock_client` without prior
  ROLLBACK is correct (the client is returned in idle-after-abort
  state). ✓
- `finalise_backfill` (line 639-641) is a best-effort `let _ =` on
  the terminal status transition. **Same orphan-row class as F1**,
  scoped to the backfill state machine. A finalise-Failed failure
  would leave the row in `running` after the data has been
  committed; the next `start({name, collection})` from another worker
  would observe `status='running'`, find the orphan dead via the
  heartbeat gap (set by `set_backfill_running` at audit.rs:541-561 —
  `last_heartbeat_at = NOW()`, `owner_session_id = pg_backend_pid()`),
  and could in principle reap. **There is no sweeper today.** This is
  a sub-finding of F1 (the orphan-`Running` class) extended to the
  backfill rail. The DDL rail is *worse* because its INSERT does not
  populate `owner_session_id` / `last_heartbeat_at` so even a
  heartbeat-driven sweeper would not know what to reap on the DDL
  side; the backfill rail is *partially* protected by the
  pid+heartbeat columns but still lacks the reaper.
- Cursor monotonicity guarantee: `update_backfill_progress` writes
  `validate_cursor = $2::bigint` keyed only on `id`; the call site
  passes `next_cursor` which the SDK derives from the highest id of
  the just-fetched batch. **There is no server-side check that
  `next_cursor > prior_cursor`.** A buggy SDK passing a stale
  next_cursor would silently rewind. This was OUT-of-scope per the
  brief (it's an SDK-trust contract); flagging here as latent.

**Verdict on §1.4:** [I41] remains correctly closed. One observed
sub-finding (finalise_backfill's `let _ =` is the same orphan class
as F1, on the backfill rail). One latent (cursor monotonicity is
SDK-trusted server-side; would require a `WHERE validate_cursor <
$2::bigint OR validate_cursor IS NULL` predicate on the UPDATE).

### 1.5 CIC retry budget — re-walked

`create_index_with_recovery_audited` at `backend/postgres.rs:385-600`.

`MAX_RETRIES = 3` → 4 iterations (attempts 0, 1, 2, 3). Per attempt:

1. Issue `CREATE INDEX CONCURRENTLY` against the pool (line 455).
2. **Ok path:**
   - `SELECT indisvalid` against the index oid (line 459-466).
   - Valid → return Ok(()). ✓
   - Invalid → write `index_retry` audit row at `Running` (lines
     476-477), best-effort transition to `Failed` (line 478-486 — five
     lines of `let _ =`), `DROP INDEX CONCURRENTLY IF EXISTS`
     (line 487), check exhaustion (line 488) → terminal `cic_failed`
     envelope or loop.
3. **Err path — fatal SQLSTATEs (`UNIQUE_VIOLATION`,
   `NOT_NULL_VIOLATION`, `FOREIGN_KEY_VIOLATION`, `CHECK_VIOLATION`)**:
   immediate audit + drop + terminal `unique_violation` envelope. No
   retry. ✓
4. **Err path — transient SQLSTATEs (`T_R_DEADLOCK_DETECTED`,
   `DISK_FULL`, `OUT_OF_MEMORY`)**: audit + drop + loop unless on
   final attempt → terminal `cic_failed` envelope. ✓
5. **Err path — anything else**: audit + drop + immediate terminal
   `cic_failed` envelope. No retry.

**The retry budget is sound.** Four attempts; classified by SQLSTATE;
partial-index cleanup on every failure; deterministic terminal
transitions. The only change since r4: nothing — postgres.rs is
untouched in this round.

The five `let _ = update_audit_status(...)` sites identified in r4
(`apply.rs:163-170`, `apply.rs:178-185`, `postgres.rs:480-487`,
`postgres.rs:521-529`, `postgres.rs:561-568`) remain in the same
shape. Same F1 family.

**Verdict on §1.5:** unchanged from r4.

### 1.6 Concurrent register_model + concurrent migration — key collision check

```
register_model:  pg_advisory_lock(
                   hashtext("zs_reg:<app_id>")::int4,
                   hashtext("register_model")::int4
                 )

migrations:      pg_try_advisory_lock(
                   hashtext("zs_mig:<app_id>")::int4,
                   hashtext(<migration_name>)::int4
                 )
```

The two key spaces share Postgres' 64-bit advisory lock space
(packed as `(int4, int4)`). PG uses `hashtext` from `pg_proc.h`, which
is `hash_text` over the cstring; it's a 32-bit hash with no
distinguishing namespace bit, so a collision between
`hashtext("zs_reg:my_app")` and `hashtext("zs_mig:other_app")` is
*possible* but astronomically unlikely (one in ~4×10⁹ per pair).

**More importantly**: the second key disambiguates. Even a
`hashtext("zs_reg:<X>") == hashtext("zs_mig:<Y>")` collision would
need ALSO `hashtext("register_model") == hashtext(<migration_name>)`
for an actual lock collision — joint probability ~1/2⁶⁴ per (X, Y,
name) triple. Effectively impossible in practice.

**Sub-finding (low / informational):** the comment at
`lock_guard.rs:201` says "every later caller hangs on
`pg_advisory_lock(zs_reg:<app>, register_model)`". That's accurate
for register_model collisions but elides the migrations advisory lock
which uses the **non-blocking** `pg_try_advisory_lock`. A migration
caller that races with a stuck register_model on a hash collision
would see `pg_try_advisory_lock` return false and surface
`err_already_running()` rather than hang — *better* failure mode
than the DDL path. Doc-only, not actionable.

**Verdict on §1.6:** no collision risk in practice. The two locks
are independently keyed and use different (blocking vs. non-blocking)
acquisition modes, which is correct given their semantic difference
(serialise DDL deploys vs. refuse concurrent backfill runs).

### 1.7 `__zeroship_migrations` table schema — recent commits

Searched the whole crate for direct edits to `__zeroship_migrations`
schema (the `CREATE TABLE IF NOT EXISTS` body at `audit.rs:198-251`):
**no commits since r4 touched it.** The schema at HEAD:

```
id                  BIGINT GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
collection          TEXT NOT NULL,
phase               TEXT NOT NULL,
change_class        TEXT NOT NULL,
change_kind         TEXT NOT NULL,
details             JSONB NOT NULL,
ddl_sql             TEXT,
created_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
updated_at          TIMESTAMPTZ NOT NULL DEFAULT NOW(),
applied_at          TIMESTAMPTZ,
applied_by_kind     TEXT NOT NULL,
applied_by_id       TEXT,
deploy_id           TEXT NOT NULL,
parent_id           BIGINT REFERENCES … (id),
schema_version      INTEGER NOT NULL,
status              TEXT NOT NULL,
error               TEXT,
duration_ms         INTEGER,
validate_cursor     BIGINT,
owner_session_id    TEXT,
last_heartbeat_at   TIMESTAMPTZ,
dead_letter_pks     JSONB,
audit_generation    BIGINT NOT NULL DEFAULT 0,
CONSTRAINT _phase_chk     CHECK (phase IN ('ddl','validation','backfill','audit')),
CONSTRAINT _class_chk     CHECK (change_class IN ('additive','compatible','destructive')),
CONSTRAINT _status_chk    CHECK (status IN (
  'pending','running','applied','applied_with_dead_letter',
  'failed','cancelled','rolled_back'
))
```

**Unchanged from r4.** No `validation_refused` status (still blocked
on F2). No `started_at`/`finished_at` decomposition (the duration
column is unwritten in practice; was carried over from the A3
proposal). No telemetry fields beyond what the proposal mandates.

Side-effect observation: the `audit_generation BIGINT NOT NULL
DEFAULT 0` column added at the Gap X commit is idempotent-added by
the `ALTER TABLE … ADD COLUMN IF NOT EXISTS` at `audit.rs:245-251`
on every `ensure_audit_table` call. That's correct (cold-start
orchestrator can re-run safely), but on a hot path it issues one
extra round-trip per `register_model` / `migrations.start` / status
read. Cosmetic — not on a per-batch path.

### 1.8 `first_row_or_internal` helper — pipeline interaction

`eda96ead` extracts the empty-RETURNING predicate into a shared helper.
Two pipeline-domain sites now route through it:

- `audit.rs:325` (`write_audit_row` — DDL writes) — was already typed
  via `ok_or_else(|| DbError::Internal { ... })`; the helper preserves
  the same `DbError::Internal` variant.
- `audit.rs:613` (`insert_backfill_running` — migration row INSERT) —
  same conversion.

**Net behaviour change:** the error message body for an empty
RETURNING shifted from `"audit: <op> returned no row"` to
`"audit: <op>: returned no row"` (extra `:` between op name and the
literal). The variant is still `DbError::Internal`; the unit test
`insert_backfill_running_empty_returning_is_internal_error` at
`audit.rs:875-885` was updated to assert the new shape.

The defensive `if id == 0 { return Err(coded("internal", ...)) }`
check at `migrations.rs:326-332` is now structurally **unreachable**:
`insert_backfill_running` raises `DbError::Internal` via
`first_row_or_internal` before any `id = 0` value can be returned.
Per the architect's note in `eda96ead`'s commit message, this site
stays on its `OpError::Coded` rail intentionally — "the helper's
signature ensures the predicate is named once" while the
migrations.rs path keeps its `coded()` flavour for SDK-facing
consistency. The check is dead-defensive code; remove only if a
follow-up audit-cleanup commit can verify nothing else in the path
can hand back `id=0`.

---

## 2. Pipeline diagram at HEAD (r5)

Functionally identical to r4 (no commits touched `migrations.rs` or
`orchestrator/register_model/`):

```
register_model_dispatch                  (orchestrator/register_model/mod.rs:63)
        │
        ├─ [fast path] is_model_registered? → resolve(undefined)
        │
        └─ exec_register_model                    (mod.rs:108)
               ▼
          bootstrap                               (bootstrap.rs:78)
            pool.get() → lock_client
            OrchestratorLockGuard::acquire        (lock_guard.rs:97)
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

Backfill orchestrator (migrations.rs) — unchanged from r4:
  exec_begin          → acquire_dedicated_client + try_advisory_lock(zs_mig:<app>, name)
                        ensure_audit_table (map_audit_bootstrap_err)
                        find_latest_backfill_row → snapshot start_generation
                        insert_backfill_running OR set_backfill_running ← no gen-check (R4-M1)
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
                        update_backfill_progress          ← R4 FIX: INSIDE tx, under row lock  ✓
                        COMMIT/ROLLBACK
                        if is_done → finalise_backfill (`let _ =`)        ← F1 family on backfill rail
                                   + release_advisory_lock + drop(client) + clear_mig_lock
```

---

## 3. R5 findings

### CRITICAL

(none)

### IMPORTANT

**R5-I1 — F1: Orphan `Running` DDL audit rows still unmoved.** (R4-I1 / R3-I1 / R2/F1 carry-forward — *unchanged at HEAD*)

- **File:** `crates/plugin-db/src/orchestrator/register_model/apply.rs:160-188`;
  `crates/plugin-db/src/audit.rs:294-326`
- **Why:** Both terminal branches in `apply.rs::run_op` end in
  `let _ = backend.update_audit_status(...).await;`. The DDL INSERT in
  `audit.rs::write_audit_row` does not populate
  `owner_session_id` / `last_heartbeat_at`, so an orphan `Running` row
  from a failed terminal UPDATE is indistinguishable from a real
  in-flight DDL by any operator scan.
- **Why: data loss / stall / cross-app impact:** No data loss; no
  stall (the OrchestratorLockGuard releases on every observable Err
  path — verified §5 below). Operator-state observability rot:
  per-deploy at least one stale `Running` row after a transient
  terminal-UPDATE failure. Blocks any future drift-detection /
  manual-approve UX. Compounds with R5-M3 (Drop-path panic — guard
  releases via Drop's sync log only).
- **Fix:**
  1. Add `owner_session_id = pg_backend_pid()::text,
     last_heartbeat_at = NOW()` to the DDL INSERT column list
     (audit.rs:297-303).
  2. Replace each `let _ = update_audit_status(...).await;` with
     `if let Err(e) = ... { tracing::warn!(audit_id = id, error = ?e,
     "audit: terminal-transition update failed; row stays Running
     until sweeper reaps"); }`. Applies to apply.rs:163-170 and
     178-185 plus the three CIC-loop sites
     (postgres.rs:478-486, 519-528, 558-567) and the
     `finalise_backfill` call at migrations.rs:639-641.
  3. Ship a heartbeat-driven sweeper modelled on
     `replication::drop_abandoned_slots` (replication.rs:421): scan
     `phase='ddl' AND status='running' AND (last_heartbeat_at IS NULL
     OR last_heartbeat_at < NOW() - INTERVAL '5 min')` and demote
     to `failed` with a synthesised error.
- **Verification:** apply.rs:163-170 (Applied `let _ =`);
  apply.rs:178-185 (Failed `let _ =`); audit.rs:297-303 (INSERT
  column list — no session/heartbeat); audit.rs:541-561 (backfill
  `set_backfill_running` *does* stamp pid+heartbeat — DDL is
  asymmetric).

**R5-I2 — F2: Orphan `Pending` audit rows from validate refusals still unmoved.** (R4-I2 / R3-I2 / R2/F2 carry-forward — *unchanged at HEAD*)

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:66-95`;
  `crates/plugin-db/src/audit.rs:229-231` (status CHECK)
- **Why:** validate.rs writes a `Pending` audit row per destructive
  op whenever `ctx.strictness != "off"`. Strict path returns Err →
  pipeline aborts → rows stay Pending. Lenient path returns Ok with
  destructive ops still in `plan.ops`; apply skips them at
  apply.rs:203 and 237 so `update_audit_status` is never called —
  rows stay Pending. The status CHECK constraint at audit.rs:229-231
  does not include `validation_refused`, so even if the pipeline
  wanted to write a terminal "refused" status it would hit
  SQLSTATE 23514.
- **Why: data loss / stall / cross-app impact:** No data loss; no
  stall. Pure observability pollution — the audit table grows
  unboundedly with phantom Pending rows.
- **Fix:**
  1. Extend `__zeroship_migrations_status_chk` (audit.rs:229-231) to
     include `validation_refused`.
  2. Add `TerminalStatus::ValidationRefused` (audit.rs:148-166) with
     `as_sql() = "validation_refused"`.
  3. Change validate.rs:67-88 to write Running and immediately
     `update_audit_status(ValidationRefused, …)` — mirrors the
     apply-layer state-machine pattern; no new `InitialStatus`
     variant needed.
- **Verification:** validate.rs:69-87 (Pending write site);
  validate.rs:90-92 (strict returns Err with no terminal transition);
  validate.rs:93-94 (lenient falls through with destructive ops
  retained); apply.rs:203 + 237 (destructive skip path — never
  reaches `update_audit_status`); audit.rs:229-231 (status CHECK
  constraint — `validation_refused` not present).

### MINOR

**R5-M1 — R4-M1: `set_backfill_running` lacks generation predicate.** (*unchanged at HEAD*)

- **File:** `crates/plugin-db/src/migrations.rs:288-306`;
  `crates/plugin-db/src/audit.rs:541-561`
- **Why:** `exec_begin` reads the snapshot then calls
  `set_backfill_running(&client, app_id, row.id)` with no
  `audit_generation` argument. An operator `migrations.reset` between
  the SELECT and the UPDATE flips the row's `audit_generation` from
  `g0` to `g0+1`. Our `set_backfill_running` runs `UPDATE … status =
  'running' WHERE id = $1::bigint` and overwrites the reset's
  `'pending'`. The reset isn't *lost* — the next commit_batch detects
  the generation mismatch and `err_reset_externally`'s — but the row
  briefly shows `status='running'` to operator-side audit scans.
- **Operator impact:** transient UI ghost (`status='running'`
  for ~one batch interval). Self-heals at next `commit_batch`.
  Cosmetic.
- **Fix:** add `AND audit_generation = $2::bigint` to
  `set_backfill_running`'s WHERE (audit.rs:546-553) and pass the
  freshly-read `row.audit_generation` from migrations.rs:303; on
  `0 rows affected`, return `err_reset_externally()` from
  `exec_begin` directly so the SDK mints a fresh wrapper. ~5 LOC.
- **Verification:** migrations.rs:288-306 (snapshot-then-set is
  non-transactional, no gen-check passed); audit.rs:546-561 (UPDATE
  keyed only by id).

**R5-M2 — F4 / R4-M2: Split lenient invariant.** (*unchanged*)

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:106`;
  `apply.rs:203, 237`
- **Why:** `check_destructive_invariant` (3ef6a170) covers the diff-
  classifier side. The skip lines at apply.rs:203 and 237 are the
  only structural enforcement of "lenient must not apply
  destructive". A type-level split — e.g. `enum ApprovedPlan {
  Strict(Vec<DiffOp>), LenientWithStripped(Vec<DiffOp>) }` with
  destructive ops removed at the validate boundary — would remove
  the possibility entirely. Current shape: defence-in-depth at one
  layer, not two.

**R5-M3 — R4-M3: `OrchestratorLockGuard::Drop` panic path still mitigated only by logs.** (*partial progress*)

- **File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:192-220`
- **Round-r5 movement:** `808a32af` strengthens the warning ("leak:"
  prefix, operator-facing consequence sentence, diagnostic
  checklist). `bd1e7ce1` defers the released-flag flip to AFTER
  the unlock await so a cancellation between flip and unlock cannot
  produce a silent leak (the Drop log now fires correctly on
  cancellation paths). `ffb1e101` adds `tracing::warn!` if the
  unlock SQL itself errored. Together these convert what r4 called
  "panic-unwind / cancellation silent leak" into "panic-unwind /
  cancellation **logged** leak with diagnostic context".
- **Residual:** the `AssertUnwindSafe` + `catch_unwind` at the
  pipeline boundary (mod.rs:85-101 in r4's recommendation) is still
  unshipped. A panic during pipeline execution → Drop logs → pooled
  client returns to pool with session-scoped lock held →
  subsequent register_model callers stall until backend session
  ends. The lock auto-releases on session close (typically pool
  recycle, tens of seconds to minutes). **The hardening reduces the
  *diagnosability* of the leak but does not eliminate the leak
  window itself.** Bumping from r4 because the silent-cancellation
  variant (R4-M3's most actionable subcase) is now structurally
  closed.

**R5-M4 — R4-M4: `pg_index` validity check string-interpolates.** (*unchanged*)

- **File:** `crates/plugin-db/src/backend/postgres.rs:459-466`
- Latent — index names are restricted by `query::build_*_indexes`.
  Fix: `query_text_params("… WHERE indexrelid = $1::regclass",
  &[qualified_idx.as_str()])`.

**R5-M5 — R4-M5: Migration v8_class `Drop` cancels audit row but strands lock.** (*unchanged*)

- **File:** `crates/plugin-db/src/v8_classes/migration.rs:84-131`;
  `crates/plugin-db/src/migrations.rs:698-726`
- The Drop finalizer spawns `exec_cancel` (audit-row UPDATE only),
  which does NOT touch the parked lock client on the worker that owns
  the run. Isolate is wedged for that `(name, collection)` until
  isolate teardown auto-releases via session close. Documented at
  migration.rs:106-108. Spawn-during-shutdown is already panic-
  caught (migration.rs:119-130). Not a worsening.

**R5-M6 — R4-M6 / F6: CIC retry-exhaustion fallback variant.** (*unchanged*)

- **File:** `crates/plugin-db/src/backend/postgres.rs:593-600`
- Unreachable in current code. Fallback returns `DbError::Configuration
  { code: "cic_configuration" }` while every other terminal arm
  returns `SchemaRefused { code: "cic_failed" }`. Cosmetic;
  defence-in-depth tripwire.

**R5-M7 (NEW r5, low) — `finalise_backfill` and CIC audit transitions share F1's `let _ =` pattern on the backfill rail.**

- **File:** `crates/plugin-db/src/migrations.rs:639-641`
  (`let _ = backend.finalise_backfill(...)`); also `postgres.rs:478-486 / 519-528 / 558-567` (CIC's three audit transitions).
- **Why:** the F1 family extends past DDL: the backfill terminal
  transition is also a `let _ =`, so a transient pool error during
  `finalise_backfill` after a successful COMMIT leaves the row in
  `running` despite the data being permanently committed. The
  backfill row *does* carry `owner_session_id` + `last_heartbeat_at`
  (audit.rs:573-579), so a future heartbeat sweeper could
  legitimately reap; but the sweeper itself doesn't exist yet.
- **Why: data loss / stall:** No data loss (data already committed),
  no stall (the advisory lock releases via `drop(client)` at
  migrations.rs:646 regardless of the finalise outcome). Same
  observability rot class as F1. Until the sweeper exists, the row's
  `status='running'` is a phantom that the next `migrations.start({name,
  collection})` call would conflict with via the find_latest_backfill_row
  check at migrations.rs:288 — depending on the prior state, this
  could either resume the row (set_backfill_running) or skip the
  restart logic and surface confusing status. Worth one tracing::warn
  + the same sweeper coverage R5-I1 proposes for DDL.
- **Fix:** same shape as R5-I1.3 — `if let Err(e) =
  backend.finalise_backfill(...).await { tracing::warn!(...) }`.
  Three-line change.
- **Verification:** migrations.rs:639-641.

**R5-M8 (NEW r5, latent) — cursor monotonicity is SDK-trusted on `update_backfill_progress`.**

- **File:** `crates/plugin-db/src/audit.rs:718-747`
- **Why:** `update_backfill_progress` runs `UPDATE … SET
  validate_cursor = $2::bigint … WHERE id = $1::bigint` with no
  predicate that `$2 > previous validate_cursor`. A buggy SDK or
  rolled-back-batch-then-retry path that re-issues commit_batch with
  a stale `nextCursor` would silently rewind the cursor.
- **Why: data loss:** narrow — the cursor advance is what the SDK
  uses to skip already-processed rows. A rewound cursor causes
  *reprocessing*, not data loss; the per-row migration must be
  idempotent (already a B1 contract). So this is risk-of-redundant-
  work, not risk-of-skip.
- **Fix:** add `AND (validate_cursor IS NULL OR validate_cursor <
  $2::bigint)` to the WHERE; on 0 rows affected, surface
  `migration_cursor_rewind` coded error so the SDK can re-snapshot.
- **Verification:** audit.rs:726-734 (UPDATE keyed only by id).
- **Severity:** latent / defensive. SDK is trusted today; would need
  to be a bug in `@zeroship/migrations` or an SDK retry-with-stale-
  state to trigger.

---

## 4. Stage invariants at HEAD (r5 update)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Guard held (#[must_use] (808a32af)); schema + audit table exist; `schema_version` monotonic | Err → `guard.release().await; return Err` |
| **plan** | Lock held | `Vec<DiffOp>` classified ([I28] diff.rs now returns DbError on Err — semantics unchanged for `compute_diff` (pure)) | Err → caller releases (mod.rs:217) |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows NOT terminalised** (R5-I2 / F2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows Running→Applied/Failed; **terminal UPDATE errors silently discarded** (R5-I1 / F1); `check_destructive_invariant` traps misclassified Drops | Err → release (line 226), then `pass1?` propagates |
| **apply pass 2** | Lock released | CIC ops run idempotently; retry loop with `pg_index` indisvalid check + audited retry log (4 attempts; deterministic SQLSTATE classification) | terminal → `SchemaRefused { code: "cic_failed" }`; fallback → `cic_configuration` (R5-M6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`set_backfill_running` lacks gen predicate** (R5-M1); insert_backfill_running raises DbError::Internal on empty RETURNING via `first_row_or_internal` (eda96ead) | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held | BEGIN; row FOR UPDATE inside tx; generation check inside tx; per-row UPDATEs; **`update_backfill_progress` inside tx under row lock** (R4 [I41] fix preserved); COMMIT/ROLLBACK; is_done → `finalise_backfill` (`let _ =`, R5-M7) + release + drop | dry-run → ROLLBACK; partial-failure → `rollback_and_return` |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release in-flight worker's mig advisory lock** (R5-M5) | benign for audit row; isolate wedged until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — closed at r4 by [I41] |
| **OrchestratorLockGuard::release** | Guard live | `pg_advisory_unlock` issued (best-effort, but **logs `tracing::warn!` on SQL error** (ffb1e101)); released flag set AFTER unlock await (bd1e7ce1); returns unlocked PooledClient | SQL errors logged not swallowed silently |
| **OrchestratorLockGuard::drop** (panic / forgotten release) | n/a | `tracing::error!` with "leak:" prefix + diagnostic checklist (808a32af); PooledClient returns with lock held; auto-release on backend session close | Documented catastrophic fallback (R5-M3); reduced silent variant |

---

## 5. Audit-row terminal-transition reliability (r5 re-tally)

Sites that discard a typed `Result` from a state-transition write:

1. **apply.rs:163-170** — DDL Applied transition (F1, R5-I1).
2. **apply.rs:178-186** — DDL Failed transition (F1, R5-I1).
3. **postgres.rs:478-486** — CIC `invalid_index_landed` Failed (F1 family).
4. **postgres.rs:519-528** — CIC `data_violation` Failed (F1 family).
5. **postgres.rs:558-567** — CIC `transient_retry` / `non_transient_failure` Failed (F1 family).
6. **migrations.rs:639-641** — backfill terminal `finalise_backfill` (R5-M7, **NEW r5**).

The R5-M7 site is functionally part of the F1 family: a typed Result
discarded at a terminal-transition write. The R5-I1 fix shape
applies uniformly; the backfill rail's heartbeat columns are already
populated so the sweeper can reap.

OrchestratorLockGuard does not protect these — the audit writes
either go through the pool (DDL Apply, CIC) or through the dedicated
mig_lock client (backfill `finalise_backfill`); the guard's
release-on-Err invariant is orthogonal to per-write error handling.

---

## 6. Concurrent register_model — re-walked under r5 hardening

The guard hardening since r4 (`#[must_use]`, deferred flag flip,
unlock-error tracing) tightens the invariant without changing the
control flow:

- **Acquisition** (lock_guard.rs:97-110): unchanged.
- **bootstrap build_ctx Err** (bootstrap.rs:132-140): explicit
  `guard.release().await`; return Err. ✓
- **plan Err** (mod.rs:209-219): `guard.release().await`; return Err. ✓
- **validate strict refusal Err** (mod.rs:201-206 → 211-219): envelope
  wrapped as `DbError::SchemaRefused`; release path runs. ✓
- **apply Pass 1 Err** (apply.rs:201-229): `guard.release().await`
  always runs (line 226) *before* `pass1?` propagates. ✓
- **apply Pass 2** (apply.rs:232-240): runs unlocked; any Err
  propagates without further lock involvement. ✓
- **Cancellation between flip-and-unlock**: previously a silent leak
  (released = true was set before the await, so Drop's catastrophic-
  path log was suppressed even though the unlock SQL was never sent).
  Now fixed (bd1e7ce1): a cancellation here keeps `released = false`,
  so Drop fires its log correctly and the operator sees the leak.
- **Unlock SQL error**: previously `let _ =`'d (silent). Now
  (ffb1e101) `tracing::warn!` with key, tag, and error. Operator can
  observe the leak; PG auto-releases at session close.
- **Panic in any stage**: sync `Drop` logs `tracing::error!` with
  "leak:" prefix + diagnostic checklist (808a32af); pooled client
  returns to pool with session lock held; auto-release on backend
  session close. **Documented; not auto-recoverable. R5-M3.**

The r4 verdict for `OrchestratorLockGuard` carries forward with one
upgrade: **the silent variants of the leak window are now logged**.
The leak window itself (between panic / forgotten-call and backend
session close) still exists; only `catch_unwind` at the pipeline
boundary would eliminate it.

---

## 7. Score

**83 / 100** (+1 vs r4: **82**)

### Movement vs r4

- **+1 for r5 guard hardening (3 commits).** `808a32af` adds
  `#[must_use]` (compile-time floor) + louder Drop log; `bd1e7ce1`
  fixes the released-flag flip ordering so cancellation can no longer
  silently leak the lock; `ffb1e101` logs unlock-SQL errors. None of
  these eliminates R5-M3 (the panic-unwind / session-close leak
  window), but they convert silent variants into observable ones.
  Worth +1 on the "concurrent register_model" dimension that
  previously sat at 88.
- **+0 for `eda96ead` (first_row_or_internal extraction).** Mechanically
  safe; preserves `DbError::Internal` contract on the two pipeline
  sites (audit.rs:325, audit.rs:613). Cosmetic message-body
  punctuation change. Doesn't move any dimension.
- **+0 for `0049d9be` (diff.rs typed-error sweep).** Mechanically safe;
  output of `compute_diff` (pure) unchanged; the three `pool.query_*`
  Err paths now produce typed `DbError` instead of `String`. The
  pipeline already wrapped the prior `String` errors via the
  surrounding orchestrator's typed rail, so SDK retry discrimination
  is *marginally* better (transient SQLSTATEs on schema introspection
  now classify) but no operator-visible behaviour shift on the
  expected paths. Net 0.
- **-0 for the new R5-M7 / R5-M8 findings.** Both are sub-findings
  of pre-existing classes (F1 family / SDK-trust). Documented; not
  worth a deduction at the IMPORTANT tier.
- **F1 (R5-I1), F2 (R5-I2), R5-M1 unchanged.** Same dispatch
  recommendation as r4: ship the F2 close (smallest LOC, biggest
  visibility win) first; R5-M1 next (3-5 LOC); F1 (R5-I1) last
  because it needs the sweeper.

### Score components

- **Strict-deploy correctness:** strong; plan/validate/apply
  boundaries crisp; guard now `#[must_use]` so accidental drops
  warn at compile time. **91/100** (was 90).
- **Lenient/off-deploy correctness:** unchanged — split invariant
  remains (R5-M2). **75/100**.
- **Audit-row state machine consistency:** weakest dimension. Two
  orphan classes (F1, F2) plus R5-M7's backfill terminal variant.
  No commits moved this. **65/100** (unchanged).
- **Backfill orchestrator:** in-tx progress UPDATE sound (R4 [I41]
  preserved); row lock semantics correct; cross-batch generation
  check intact. R5-M1 residual gap-X variant; R5-M7 finalise-
  let-underscore variant; R5-M8 SDK-trusted cursor monotonicity
  latent. **80/100** (was 82 — minor down for the two latent
  findings).
- **CIC recovery loop:** robust; deterministic SQLSTATE
  classification; bounded retry; partial-index cleanup. Unchanged.
  **90/100**.
- **Concurrent `register_model`:** guard hardened in three commits
  this round. Silent-leak variants converted to observable ones.
  Panic-unwind leak window remains (R5-M3) but is now structurally
  diagnostic. **90/100** (was 88).

### Comparison to r4 (82)

The r5 delta is the three guard-hardening commits, which are exactly
the "raise the observability floor on a documented-but-unmitigated
issue" pattern that the r4 review surfaced as the residual cost on
R4-M3. The +1 reflects that the silent-cancellation variant of the
panic-unwind class is now closed; the panic-unwind class itself
(catch_unwind) is not.

The score floor projected in r4 — "ship R4-I2 (~15 LOC) + R4-M1
(~3-5 LOC) + the tracing::warn part of R4-I1 to reach ~87-88" — is
unchanged at HEAD. Nothing in this round shipped F1/F2/M1, so the
score floor is still gated on the same three items. The plus-one
from guard hardening is "free" progress on a dimension that wasn't
the bottleneck.

Suggested next attack order (unchanged from r4):

1. **R5-I2** — `TerminalStatus::ValidationRefused` + CHECK constraint
   extension + flip validate.rs Pending writes to immediate-Refused.
   ~15 LOC, closes F2.
2. **R5-M1** — `audit_generation = $2` predicate on
   `set_backfill_running` + 0-rows-affected → `err_reset_externally`
   in exec_begin. ~5 LOC, closes the last gap-X variant.
3. **R5-I1** — DDL row session/heartbeat columns +
   `tracing::warn!` on all six (was five) silent UPDATE discards +
   heartbeat sweeper. ~30-50 LOC. The R5-M7 site joins R5-I1 in this
   bucket — same fix shape, one extra `tracing::warn!` call site.

After (1) + (2) + extending the `tracing::warn!` part of (3) to the
six sites: score floor moves to ~87-88. Deductions then concentrate
in R5-M2 (type-split for lenient) and R5-M3 (catch_unwind at
pipeline boundary). Both are next-stratum refactors.
