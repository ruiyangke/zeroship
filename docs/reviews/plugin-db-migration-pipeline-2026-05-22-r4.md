# plugin-db: Migration / DDL Pipeline Correctness Review (R4)

**HEAD:** post-`37e61803` · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72/100) · r2 (81/100) · r3 (81/100, cycle 02:50, unchanged)

---

## 0. Scope of this round

r3 closed with the recommendation to ship one of `R3-I1` (DDL orphan
Running), `R3-I2` (validate-refused orphan Pending), or `R3-I3`
(reset-clobber race on the post-COMMIT audit progress write) as the
fastest mover. The visible delta at HEAD vs. r3 is:

| Commit | Lens fix | r3 ID closed |
|---|---|---|
| `37e61803` | `update_backfill_progress` moved BEFORE COMMIT | **R3-I3 / [I41]** |
| `cbd12944` | `OrchestratorLockGuard` RAII extraction | (already in r3 cycle) |
| `dec2bd42` | migrations.rs regression recovery (Internal-arm context prefix restored) | error-ux r3 carry |
| `ed697c45` | `map_audit_bootstrap_err` helper centralised | (already in r3 cycle) |
| `3ef6a170` | `DropColumn`/`DropIndex` invariant hard-error | r2-fixed |

Net structural change since r3: **one** — the R3-I3 reset-clobber close.
F1 (orphan `Running` DDL audit rows) and F2 (orphan `Pending`
validate-refused rows) remain unmoved.

---

## 1. Verify [I41] is fully closed

### 1.1 The fix at HEAD (migrations.rs:584-619)

The `update_backfill_progress` call now sits **between** the data-row
UPDATE loop (lines 521-582) and the final `COMMIT/ROLLBACK` (lines
615-619). Concretely:

```text
client_exec("BEGIN")                       (line 482)
lock_audit_row_for_update(client, audit_id) (line 497)   ← FOR UPDATE row lock
  if status == "cancelled"  → rollback_and_return; err_cancelled_mid_run
  if audit_generation != start_generation → rollback_and_return; err_reset_externally
for upd in updates_arr:
  client_exec(UPDATE schema.table SET … WHERE id = $1)    (line 578)
  on Err → rollback_and_return; coded_db("migration row UPDATE")
if !dry_run:
  update_backfill_progress(client, audit_id, next_cursor, dlp, processed)  (line 595)
    on Err → rollback_and_return; coded_db("audit row update")             (line 606)
client_exec(COMMIT | ROLLBACK)                             (line 616)
  on Err → return_lock_client; coded_db(…)                                 (line 617-618)
if is_done:
  finalise_backfill; release_advisory_lock; drop(client); clear_mig_lock
return_lock_client(client)
```

**Checkpoints from the review checklist:**

- **Audit progress UPDATE happens before COMMIT.** ✓ Line 595 issues
  `update_backfill_progress`; line 616 then issues the `COMMIT` /
  `ROLLBACK`. The progress write is now part of the same transaction
  as the data-row UPDATEs.
- **Row lock from `lock_audit_row_for_update` still held when the
  progress UPDATE runs.** ✓ The lock was acquired at line 497 via
  `SELECT … FOR UPDATE` (audit.rs:702-711) inside the BEGIN. Postgres
  holds row locks until transaction end; line 595 still sits inside
  that transaction. Operator `migrations.reset` from another
  connection runs `UPDATE … audit_generation = audit_generation + 1`
  on the same row (audit.rs:631-647) and blocks until our COMMIT
  releases the row lock.
- **Error path calls `rollback_and_return`, not `return_lock_client`.**
  ✓ migrations.rs:605-608. The helper (lines 476-479) issues
  `ROLLBACK` then parks the client back in `ctx.mig_lock`. Pre-fix the
  call site would have committed (or been outside the transaction
  entirely); the new shape correctly rolls back any partial data-row
  UPDATEs already issued in this batch. **No partial-state leak on
  audit-row UPDATE failure.**
- **`audit_generation` predicate in `update_backfill_progress` WHERE.**
  ✗ Not added — and **not needed** given the lock semantics. See §1.2.
- **COMMIT-failure path.** ✓ migrations.rs:616-619. On COMMIT failure
  Postgres implicitly rolls back the transaction (PG error class
  semantics — a failed COMMIT aborts the tx server-side), so the
  explicit `return_lock_client` without prior ROLLBACK is correct.
  The client is returned to an idle (post-aborted-tx) state.

### 1.2 Why the missing generation predicate is correct under the new ordering

Pre-`37e61803`: `update_backfill_progress` ran *after* COMMIT, on the
unlocked connection. A concurrent `reset_backfill_row_pool` (audit.rs:
631-647) could fire between COMMIT and the progress UPDATE, bumping
`audit_generation` and zeroing the cursor; our subsequent UPDATE wrote
back the new cursor over the just-cleared row. The reset was silently
clobbered. **That window required either a predicate or in-transaction
ordering.**

Post-`37e61803`: the progress UPDATE runs *inside* the transaction
that holds the row lock from `lock_audit_row_for_update` (FOR UPDATE
acquired at line 497). The lock is released by PG only when our
`COMMIT` / `ROLLBACK` at line 616 finishes. Any concurrent
`reset_backfill_row` UPDATE waits behind the row lock and only sees
the row *after* COMMIT — at which point its own UPDATE legitimately
overrides the freshly-written progress (which is exactly the
operator's intent: reset means "throw away whatever's there"). The
race is now serialised correctly.

For the *next* batch in the same logical run, `lock_audit_row_for_update`
re-reads `audit_generation` and compares against the snapshot's
`start_generation` (migrations.rs:515). If a reset slipped through
between two batches, the generation mismatch triggers
`err_reset_externally` *before* any further data UPDATEs run. The
in-transaction lock + cross-batch generation check together cover the
operator-reset surface end-to-end. **The predicate would be defensive
duplication; the typed-state-machine layer already enforces the
invariant.**

### 1.3 Residual sub-finding (low) — `set_backfill_running` does not re-check generation

- **File:** `crates/plugin-db/src/migrations.rs:302-306`;
  `crates/plugin-db/src/audit.rs:541-561`
- **Why:** `exec_begin` reads the snapshot via
  `find_latest_backfill_row` (line 288), then calls
  `set_backfill_running` (line 303) which issues
  `UPDATE … status='running' WHERE id = $1::bigint`. No
  `audit_generation` predicate. If an operator's `reset` fires in the
  window between the read and the `set_backfill_running` UPDATE, the
  bumped generation is preserved (reset's UPDATE wrote it on the same
  row), but `set_backfill_running` flips status from `'pending'`
  (just set by reset) back to `'running'`. The snapshot's
  `start_generation = g0` is stale; the row's generation is now
  `g0+1`. On the next `commit_batch`, `lock_audit_row_for_update`
  reads `g0+1` and `start_generation != audit_generation` triggers
  `err_reset_externally`. The protection holds, but **the run was
  briefly visible as `'running'` after a reset that the operator
  expected to leave as `'pending'`**.
- **Operator impact:** UI scrapes of `status='running'` between the
  reset and the first `commit_batch` show a phantom in-flight run for
  a name the operator just reset. Self-heals on the next commit_batch
  (~one batch interval). Cosmetic — but a tighter guard would also
  let `exec_begin` itself surface the reset rather than waiting for
  commit_batch.
- **Fix:** add `AND audit_generation = $2::bigint` to
  `set_backfill_running`'s WHERE (passing the just-read
  `row.audit_generation`); on `0 rows affected`, return
  `err_reset_externally` from `exec_begin` directly. Or, alternatively,
  thread the find+set into a single `BEGIN; SELECT FOR UPDATE; UPDATE;
  COMMIT` block so the snapshot can't drift.
- **Verification:** migrations.rs:288-306 (snapshot-then-update is not
  in a transaction); audit.rs:546-561 (UPDATE keyed only by `id`).

**Verdict on [I41]:** **closed correctly** at the post-COMMIT race
boundary. Sub-finding above is a narrower, lower-severity variant of
the same gap-X family, not a re-open of [I41] itself.

---

## 2. Pipeline diagram at HEAD (r4)

```
register_model_dispatch                  (orchestrator/register_model/mod.rs:63)
        │
        ├─ [fast path] is_model_registered? → resolve(undefined)
        │
        └─ exec_register_model                    (mod.rs:108)
               ▼
          bootstrap                               (bootstrap.rs:78)
            pool.get() → lock_client
            OrchestratorLockGuard::acquire        (lock_guard.rs:87)
            ├── build_ctx: ensure_app_schema / ensure_audit_table /
            │     next_schema_version / expand declared_indexes
            │   ──Err──► guard.release().await; return Err(e)
            └── Ok → (RegisterContext, OrchestratorLockGuard<'p>)
               ▼
          compute_plan          ──Err──► mod.rs:217 guard.release()
               ▼
          validate
            destructive + strictness != "off" → write Pending audit rows
            strict   → Err(envelope_json)              ← Pending rows ORPHAN (F2)
            lenient  → Ok(ApprovedPlan{ ops })         ← Pending rows ORPHAN (F2)
            off      → Ok(ApprovedPlan{ ops })
          strict Err → wrap as DbError::SchemaRefused; release guard
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

Backfill orchestrator (migrations.rs):
  exec_begin          → acquire_dedicated_client + try_advisory_lock(zs_mig:<app>, name)
                        ensure_audit_table (map_audit_bootstrap_err — ed697c45)
                        find_latest_backfill_row → snapshot start_generation
                        insert_backfill_running OR set_backfill_running   ← no gen-check (§1.3)
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
                        update_backfill_progress          ← R4 FIX: now INSIDE tx, under row lock  ✓
                        COMMIT/ROLLBACK                   ← row-lock released here
                        if is_done → finalise_backfill + release_advisory_lock + drop(client) + clear_mig_lock
```

### Audit-row state machines at HEAD

**DDL** (validate + apply):
```
                              ┌─── Pending  (validate-refused destructive)  ← terminal in practice (F2 orphan)
                              │
write_audit_row ───► Running ─┼── update_audit_status ──► Applied
                              │                       └─► Failed
                              │
                              └── (worker dies / update_audit_status error) ──► Running FOREVER (F1)
```

**Backfill** (migrations) — R4 update:
```
                                       ┌── progress (update_backfill_progress IN-TX, ROW LOCKED) ──► Running   [R4]
                                       ├── operator reset (audit_generation += 1) ──► Pending
INSERT phase='backfill' status='running' ─┤── operator cancel ──► Cancelled
                                       └── commit_batch(isDone) ──► Applied / AppliedWithDeadLetter / Failed / Cancelled
```

---

## 3. Status of prior findings at HEAD

| Round | ID | Status | Evidence |
|---|---|---|---|
| R1/C1 | replication schema/publication case mismatch | FIXED (r2) | `a00c41fd` |
| R1/C2 | `DropColumn`/`DropIndex` silent no-op | FIXED (r2) | `3ef6a170` — apply.rs:62, 262 |
| R1/M3 | bootstrap lock leak on ensure_*_table error | FIXED (r2) | `3bb41fa1` → guard.release() at bootstrap.rs:137 |
| R2/F1 / R3-I1 | orphan `Running` DDL audit rows | **OPEN** | apply.rs:163-186 — `let _ = backend.update_audit_status(...).await;` |
| R2/F2 / R3-I2 | orphan `Pending` validate-refused rows | **OPEN** | validate.rs:67-87; audit.rs:229-231 status CHECK |
| R2/F3 / R3-I3 / [I41] | reset-clobber race on post-COMMIT progress | **CLOSED at r4** | `37e61803` — migrations.rs:594-609 |
| R2/F4 / R3-M1 | split lenient invariant | **OPEN, defence-in-depth via 3ef6a170** | validate.rs:106; apply.rs:203, 237 |
| R2/F5 | no manual-approve / drift UX | OPEN, scoped out | A2 proposal scope |
| R2/F6 / R3-M5 | CIC retry-exhaustion fallback variant choice | OPEN, cosmetic | postgres.rs:595-602 |
| R2/F7 / R3-M2 | concurrent register_model — panic-unwind leak | OPEN, documented | lock_guard.rs:159-181 |
| R2/F8 / R3-M4 | `exec_cancel` doesn't release mig advisory lock | OPEN, documented | migrations.rs:698-726; v8_classes/migration.rs |
| R2/F9 / R3-M3 | `pg_index` validity check interpolates SQL | OPEN, latent | postgres.rs:461-464 |
| **NEW r4** | `set_backfill_running` lacks generation predicate | **OPEN (low)** | §1.3 above |

---

## 4. R4 findings

### CRITICAL

**(none — R1 high-blast-radius items remain fixed; R3-I3 closed; F1/F2
remain IMPORTANT-tier, not CRITICAL.)**

### IMPORTANT

**R4-I1 — Orphan `Running` DDL audit rows still unobservable, unsweepable.** (carry-forward of R2/F1 / R3-I1, no progress)

- **File:** `crates/plugin-db/src/orchestrator/register_model/apply.rs:160-188`;
  `crates/plugin-db/src/audit.rs:294-332` (DDL `write_audit_row` site)
- **Why:** Both terminal branches at apply.rs:163-186 end with
  `let _ = backend.update_audit_status(...).await;`. If that UPDATE
  fails (transient pool error, statement timeout, connection drop,
  process kill between DDL success and the terminal write), the audit
  row stays `Running`. The DDL write path does **not** populate
  `owner_session_id` / `last_heartbeat_at` (audit.rs:294-332 INSERTs
  only ten columns; session/heartbeat stay NULL), so an operator
  scanning `__zeroship_migrations WHERE phase='ddl' AND status='running'`
  cannot distinguish a real in-flight DDL from an orphan. The
  backfill path *does* populate `pg_backend_pid()` + `NOW()` via
  `set_backfill_running` (audit.rs:541-561); DDL is asymmetric.
- **Why: data loss / stall / cross-app impact:** No data loss — the
  audit row is observability-only. No cross-app stall —
  `OrchestratorLockGuard::release` runs on every observable Err path
  (verified in §5). The damage accumulates in *operator state
  visibility*: per-deploy after a transient terminal-UPDATE failure,
  at least one stale `Running` row that the next deploy's audit scan
  can't reconcile. Compounds with R4-M2 (panic-unwind): if a panic
  during `apply()` unwinds before `guard.release().await`, the guard's
  sync Drop emits `tracing::error!` but the audit row also stays
  `Running` — double signal, neither auto-recoverable.
- **Fix:**
  1. Add `owner_session_id = pg_backend_pid()::text,
     last_heartbeat_at = NOW()` to the DDL INSERT column list in
     audit.rs:297-303.
  2. Replace `let _ = backend.update_audit_status(...).await;` with
     `if let Err(e) = backend.update_audit_status(...).await {
     tracing::warn!(audit_id = id, error = ?e, "audit:
     terminal-transition update failed; row stays Running until
     sweeper reaps"); }`.
  3. Ship a sweeper modelled on `replication::drop_abandoned_slots`
     (replication.rs:421): scan `phase='ddl' AND status='running' AND
     (last_heartbeat_at IS NULL OR last_heartbeat_at < NOW() -
     INTERVAL '5 min')` and demote to `failed` with synthesised error.
- **Verification:** apply.rs:163-170 (`let _ =` on Applied transition);
  apply.rs:178-185 (`let _ =` on Failed transition); audit.rs:297-303
  (DDL INSERT column list — no session/heartbeat); audit.rs:541-561
  (backfill `set_backfill_running` *does* stamp pid+heartbeat).

**R4-I2 — Orphan `Pending` audit rows from validate refusals.** (carry-forward of R2/F2 / R3-I2, no progress)

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:66-95`;
  `crates/plugin-db/src/audit.rs:229-231` (status CHECK)
- **Why:** validate.rs writes a `Pending` row per destructive op
  whenever `ctx.strictness != "off"`. In `strict` mode validate
  returns Err and the pipeline aborts — the Pending row persists with
  no terminal transition. In `lenient` mode validate returns Ok with
  the destructive ops still in `plan.ops`; apply skips them at
  apply.rs:203 and 237 so `update_audit_status` is never called.
  Either way: an orphan `Pending` row per refused destructive op per
  deploy, accumulating forever.
- **Why: data loss / stall / cross-app impact:** No data loss; no
  stall. Pure operator state pollution — the audit table grows
  unboundedly with phantom Pending rows. Any future "manual-approve
  destructive op" or "drift detection" UX is blocked on this: queries
  filtering `WHERE status='pending'` cannot distinguish "actionable
  pending awaiting approval" from "historical refusal already declined
  in deploy N".
- **Fix:**
  1. Extend the `__zeroship_migrations_status_chk` CHECK constraint
     (audit.rs:229-231) to include `validation_refused`.
  2. Add `TerminalStatus::ValidationRefused` (audit.rs:148-166) with
     `as_sql() = "validation_refused"`.
  3. In validate.rs:67-88, change the writes to `InitialStatus::Running`
     followed by an immediate `update_audit_status(ValidationRefused, …)` —
     mirrors the apply-layer state-machine pattern; no new `InitialStatus`
     variant needed.
- **Verification:** validate.rs:74-87 (Pending write site); validate.rs:90-94
  (strict returns Err, lenient falls through with no terminal); apply.rs:203, 237
  (destructive skip — no terminal transition); audit.rs:229-231 (status
  CHECK constraint).

### MINOR

**R4-M1 — `set_backfill_running` has no generation predicate, allowing a phantom `running` flash after an operator reset.** (NEW in r4 — §1.3)

- **File:** `crates/plugin-db/src/migrations.rs:288-306`;
  `crates/plugin-db/src/audit.rs:541-561`
- **Why:** `exec_begin` reads the snapshot, captures `start_generation
  = row.audit_generation`, then calls `set_backfill_running` with no
  generation predicate. If an operator's `migrations.reset` UPDATE
  fires between the SELECT and the UPDATE, the run's snapshot is
  stale but `set_backfill_running` still flips `status` back to
  `'running'`. Detection deferred to the first `commit_batch`.
- **Operator impact:** transient cosmetic UI ghost (`status='running'`
  for a name the operator just reset). Self-heals at next
  `commit_batch` via the existing Gap-X generation guard.
- **Fix:** add `AND audit_generation = $2::bigint` to
  `set_backfill_running`'s WHERE; 0-row UPDATE → fail `exec_begin`
  with `err_reset_externally`. Closes the analogous window at run
  start. Three-line change.
- **Verification:** migrations.rs:288-306 (snapshot-then-set is
  non-transactional); audit.rs:546-561 (UPDATE keyed only by id).

**R4-M2 — Split lenient invariant unchanged.** (R2/F4 / R3-M1)

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:106`;
  `apply.rs:203, 237`
- The `check_destructive_invariant` tripwire (`3ef6a170`) covers the
  diff-classifier side of the gate. The skip lines at apply.rs:203
  and 237 are the only structural enforcement of "lenient must not
  apply destructive". A type-level split — e.g.
  `enum ApprovedPlan { Strict(Vec<DiffOp>), LenientWithStripped(Vec<DiffOp>) }`
  with destructive ops removed at the validate boundary — would
  remove the possibility entirely. Current shape: defence-in-depth at
  one layer, not two.

**R4-M3 — `OrchestratorLockGuard::Drop` panic path documented but unmitigated.** (R2/F7 / R3-M2)

- **File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:159-181`
- Panic during pipeline execution → sync `Drop` runs → logs
  `tracing::error!` → pooled connection returns with session-scoped
  lock held → every subsequent caller blocks on
  `pg_advisory_lock(zs_reg:<app>, register_model)` until the backend
  session ends. The `AssertUnwindSafe` + `catch_unwind` fix at
  mod.rs:85-101 remains unshipped.

**R4-M4 — `pg_index` validity check still string-interpolates.** (R2/F9 / R3-M3)

- **File:** `crates/plugin-db/src/backend/postgres.rs:461-464`
- Latent: index names are restricted by `query::build_*_indexes`. Fix
  is `query_text_params("… WHERE indexrelid = $1::regclass",
  &[qualified_idx.as_str()])`.

**R4-M5 — Migration v8_class `Drop` cancels audit row but strands lock.** (R2/F8 / R3-M4)

- **File:** `crates/plugin-db/src/v8_classes/migration.rs:84-131`;
  `crates/plugin-db/src/migrations.rs:698-726`
- `Migration::drop` calls `exec_cancel` (audit-row UPDATE only);
  does NOT release the parked lock client. Isolate is wedged for the
  migration name until isolate teardown (which ends the backend
  session and auto-releases the lock). Documented at
  v8_classes/migration.rs:106-108.

**R4-M6 — CIC retry-exhaustion fallback variant choice.** (R2/F6 / R3-M5)

- **File:** `crates/plugin-db/src/backend/postgres.rs:595-602`
- Unreachable in current code (the retry loop always returns from
  inside). The fallback returns `DbError::Configuration { code:
  "cic_configuration" }` while every other terminal arm returns
  `SchemaRefused { code: "cic_failed" }`. Cosmetic; defence-in-depth
  tripwire.

---

## 5. Concurrent `register_model` — re-walked under OrchestratorLockGuard

Lock key: `pg_advisory_lock(hashtext('zs_reg:<app>')::int4,
hashtext('register_model')::int4)` — session-scoped on a dedicated
pooled client.

Walk-through (all paths must end in either `guard.release()` /
`into_held()` or a logged Drop):

- **Acquisition** (lock_guard.rs:87-100): `acquire` runs
  `backend.acquire_advisory_lock`. Err → client drops at the
  acquisition site; no lock held. Ok → guard owns the lock.
- **bootstrap build_ctx Err** (bootstrap.rs:132-140): explicit
  `guard.release().await`; return Err. ✓
- **plan Err** (mod.rs:209-219): `guard.release().await`; return Err. ✓
- **validate strict refusal Err** (mod.rs:201-206 → 211-219): envelope
  wrapped as `DbError::SchemaRefused`; falls through the same
  release-on-Err path as plan. ✓
- **apply Pass 1 Err** (apply.rs:201-229): the async block captures
  `Result`; `guard.release().await` always runs (line 226) *before*
  `pass1?` (line 229) propagates. The release runs **unconditionally
  on the Pass 1 outcome**. ✓
- **apply Pass 2** (apply.rs:232-240): runs unlocked; any Err
  propagates without further lock involvement. ✓
- **Panic in any stage**: sync `Drop` logs `tracing::error!` with
  `key` and `tag` (lock_guard.rs:159-181); pooled client returns to
  pool with session lock held; auto-release on backend session close.
  Documented; not auto-recoverable. **R4-M3.**

**Two concurrent `register_model` calls** for the same app:

1. Caller A acquires the lock on Pool client A1.
2. Caller B calls `pool().get()` (bootstrap.rs:103) — gets a different
   pooled client B1.
3. B1's `acquire_advisory_lock` (lock_guard.rs:93) issues blocking
   `pg_advisory_lock(...)` — Postgres parks the call on the lock's
   wait queue.
4. A finishes — `apply.rs:226 guard.release()` runs `pg_advisory_unlock`
   on A1.
5. Postgres wakes B1; the lock transfers; B's pipeline proceeds.

**Verdict on §5:** no regression. The guard centralises the
release-on-every-Err invariant; the only outstanding leak class is
panic-unwind (R4-M3). The r3 verdict carries forward unchanged.

---

## 6. Schema strictness path — audit-row stranding

| Strictness | Destructive present? | Audit rows written | Terminal status reached? |
|---|---|---|---|
| `strict` (default) | yes | one Pending per destructive op (validate.rs:67-87) | **No** — validate returns Err; pipeline aborts; rows stay Pending |
| `strict` | no | none for refusal; Apply path writes Running → Applied/Failed | yes (subject to R4-I1) |
| `lenient` | yes | one Pending per destructive op (same write) | **No** — apply.rs:203, 237 skip destructive ops; no terminal UPDATE for the Pending row |
| `lenient` | no | none for refusal; Apply path writes Running → Applied/Failed | yes (subject to R4-I1) |
| `off` | yes | **none** (validate's destructive branch is skipped) | n/a — destructive ops reach apply and execute normally |
| `off` | no | none for refusal; Apply path writes Running → Applied/Failed | yes (subject to R4-I1) |

**Stranding pattern:**

- `strict` + destructive present: row created in `Pending`, never
  transitioned. Persistent.
- `lenient` + destructive present: same shape — row created in
  `Pending`, never transitioned. Persistent.
- `off` + destructive present: no audit row written for the refusal
  (because there *is* no refusal); the apply path writes its own
  Running → Applied/Failed row. Clean.

R4-I2 is the structural fix: extend the state machine with
`ValidationRefused` and transition the Pending rows to it immediately
at validate.rs:67-87.

A subtle related concern: **validate.rs writes Pending rows even
through the lenient path that is going to silently drop the ops**. An
operator inspecting the audit table sees `phase='ddl' status='pending'`
with no follow-up; they have no way to tell from the audit row
whether the refusal was "strict-refused, deploy aborted" or
"lenient-refused, deploy continued silently". A `notes` /
`refusal_mode` field on the row body, or the `ValidationRefused`
terminal variant proposed above, would also disambiguate.

---

## 7. CIC retry budget — `create_index_with_recovery_audited`

**Loop:** `for attempt in 0..=MAX_RETRIES` where `MAX_RETRIES = 3`
(postgres.rs:398, 456) → 4 iterations total (attempts 0, 1, 2, 3).

**Per attempt:**
1. Issue `CREATE INDEX CONCURRENTLY` (postgres.rs:457).
2. **Success** (`Ok(_)`):
   - `SELECT indisvalid` check (line 462).
   - If valid → return `Ok(())`. ✓
   - If invalid → write audit row (`invalid_index_landed`) at Running,
     transition to Failed (best-effort `let _ =` at lines 480-487 —
     same `let _` pattern as R4-I1 but on a different code path);
     `DROP INDEX CONCURRENTLY IF EXISTS`; loop unless this was the
     last attempt → terminal refusal envelope.
3. **Err(e)** with `SqlState::UNIQUE_VIOLATION | NOT_NULL_VIOLATION |
   FOREIGN_KEY_VIOLATION | CHECK_VIOLATION` → `fatal = true`: write
   audit row (`data_violation`) Running→Failed; drop the partial
   index; immediate refusal envelope (no retry). ✓
4. **Err(e)** with `SqlState::T_R_DEADLOCK_DETECTED | DISK_FULL |
   OUT_OF_MEMORY` → `transient = true`: write audit row
   (`transient_retry`) Running→Failed; drop partial; loop unless last
   attempt.
5. **Err(e)** otherwise → `transient = false`: write audit row
   (`non_transient_failure`) Running→Failed; drop partial; terminal
   refusal envelope.

**Fallback (postgres.rs:595-602):** if the loop exits without a
terminal `return` — invariant breach — returns
`DbError::Configuration { code: "cic_configuration" }`. **Currently
unreachable** because every branch above either returns directly or
loops (and the loop body always reaches a return on the last
iteration). Cosmetic variant choice (R4-M6).

**Audit-row hygiene inside the loop:** every iteration's audit row is
written as `InitialStatus::Running` and immediately transitioned to
`TerminalStatus::Failed` via `update_audit_status`. The latter is
`let _ =`'d (lines 480-487, 521-529, 561-568) — same orphan-row risk
class as R4-I1. The risk is *narrower* here because the audit row's
`change_kind = "index_retry"` makes the orphan distinguishable from a
genuine in-flight DDL row, and the loop holds the pool short enough
that a transient pool error is unlikely between INSERT and the
follow-up UPDATE. Still, it's the same pattern: the typed Result is
discarded. The R4-I1 fix should apply uniformly to all five
`let _ = update_audit_status(...)` sites (apply.rs:163-170 and 178-185,
plus the three CIC-loop sites).

**Verdict on §7:** retry budget is sound (4 attempts cap; deterministic
classification by SQLSTATE; partial-index cleanup via
`DROP INDEX CONCURRENTLY IF EXISTS` after every failure; fatal
classification covers all four data-violation SQLSTATEs the index
build can hit). Two narrow improvements: visibility on the discarded
UPDATE error (same as R4-I1), and the unreachable fallback variant
(R4-M6 cosmetic).

---

## 8. Stage invariants at HEAD (r4 update)

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | Guard held; schema + audit table exist; `schema_version` monotonic | Err → `guard.release().await; return Err` |
| **plan** | Lock held | `Vec<DiffOp>` classified | Err → caller releases (mod.rs:217) |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows NOT terminalised** (R4-I2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows Running→Applied/Failed; **terminal UPDATE errors silently discarded** (R4-I1); `check_destructive_invariant` traps misclassified Drops | Err → release, then `pass1?` propagates |
| **apply pass 2** | Lock released | CIC ops run idempotently; retry loop with `pg_index` indisvalid check + audited retry log (4 attempts; deterministic classification) | terminal → `SchemaRefused { code: "cic_failed" }`; fallback → `cic_configuration` (R4-M6) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`set_backfill_running` lacks gen predicate** (R4-M1) | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held | BEGIN; row FOR UPDATE inside tx; generation check inside tx; per-row UPDATEs; **`update_backfill_progress` inside tx under row lock** (R4 fix); COMMIT/ROLLBACK; is_done → finalise + release + drop | dry-run → ROLLBACK; partial-failure → `rollback_and_return` |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release in-flight worker's mig advisory lock** (R4-M5) | benign for the audit row; isolate wedged until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — closed at r4 by [I41] for the post-COMMIT window |
| **OrchestratorLockGuard::release** | Guard live | Best-effort `pg_advisory_unlock`; returns unlocked PooledClient | SQL errors swallowed |
| **OrchestratorLockGuard::drop** (panic path) | n/a | `tracing::error!`; PooledClient returns with lock held | Documented catastrophic fallback (R4-M3) |

---

## 9. Audit-row terminal-transition reliability (re-tally for r4)

Sites that discard a typed `Result` from `update_audit_status`:

1. **apply.rs:163-170** — DDL Applied transition.
2. **apply.rs:178-186** — DDL Failed transition.
3. **postgres.rs:480-487** — CIC `invalid_index_landed` Failed transition.
4. **postgres.rs:521-529** — CIC `data_violation` Failed transition.
5. **postgres.rs:561-568** — CIC `transient_retry` / `non_transient_failure` Failed transition.

All five sites silently swallow the `Result`. The R4-I1 fix
(`tracing::warn!` on Err + DDL row session/heartbeat columns + reaper)
covers sites 1-2 directly and should be extended to sites 3-5 by
analogy: the same `tracing::warn!(audit_id = id, error = ?e, ...)`
shape suffices, since CIC retry rows already carry enough context
(`reason` discriminator in `details`) for an operator scan to filter
on `change_kind = 'index_retry'`.

OrchestratorLockGuard does not protect against these — audit-row
writes go through the pool, not the lock client. Independent
observability work item.

---

## 10. Score

**82 / 100** (+1 vs r3: **81**)

### Movement vs r3

- **+2 for R3-I3 closure** — the post-COMMIT reset-clobber race is
  closed correctly. Verified end-to-end in §1: progress UPDATE inside
  tx, row lock held, error path rolls back, COMMIT-failure path is
  PG-correct (implicit server-side rollback). The fix is the
  preferred (a) option from the r3 recommendation and removes the
  need for the secondary (b) predicate-based defence.
- **-1 for R4-M1 (NEW)** — the same gap-X family of bug at
  `set_backfill_running` (snapshot-then-update without generation
  predicate) was noticed during the §1 walk-through. Self-healing at
  next commit_batch but produces a transient `status='running'` UI
  ghost for the operator's just-reset row. Narrow; cosmetic; one of
  the easiest fixes.
- **F1 (R4-I1) and F2 (R4-I2) unchanged.** The pattern observation
  hasn't moved: each round identifies the same two state-machine
  orphans; each round they don't ship. Cumulative trajectory: the
  pipeline now has one less *race* but the same two *state-machine
  hygiene* gaps that block any future drift-detection / manual-approve
  UX.

### Score components

- **Strict-deploy correctness:** strong. Plan/validate/apply
  boundaries crisp; lock release on every observable Err path
  (verified §5). **90/100**.
- **Lenient/off-deploy correctness:** unchanged — split invariant
  remains (R4-M2). **75/100**.
- **Audit-row state machine consistency:** weakest dimension. Two
  orphan classes (R4-I1 DDL Running; R4-I2 Pending). The R3-I3
  closure removed one (the reset-clobber-induced ghost), so this
  dimension improves from r3. **65/100** (was 60/100).
- **Backfill orchestrator:** the in-tx progress UPDATE is sound; row
  lock semantics correct; cross-batch generation check intact.
  Residual R4-M1 (`set_backfill_running` predicate) is the only
  outstanding gap-X variant. **82/100** (was 75/100).
- **CIC recovery loop:** robust; deterministic SQLSTATE
  classification; bounded retry; partial-index cleanup. Same as r3.
  **90/100**.
- **Concurrent `register_model`:** guard-centralised; panic-unwind is
  the only outstanding leak class (R4-M3). **88/100**.

### Comparison to r3 (81)

The r4 delta is the one shipped state-machine hygiene fix that the r3
review specifically called for: [I41] is closed, correctly and
minimally, with no regression to the typed-error rail or the guard
invariant. The +1 net (+2 closure, -1 new low-severity find) reflects
the closure-minus-discovery balance.

The score floor identified in r3 — "ship R3-I1 / R3-I2 / R3-I3 to
reach ~88" — is now one item shorter. Shipping R4-I2 (validate's
Pending terminalisation; ~15 LOC) would close the next biggest
audit-row orphan class and bring the score into the high 80s; R4-I1
(DDL session/heartbeat + reaper; ~30-50 LOC including sweeper) is the
heavier item.

Suggested next attack order:

1. **R4-I2** — extend `TerminalStatus` with `ValidationRefused`;
   update the CHECK constraint; flip validate.rs Pending writes to
   Running + immediate-Refused. Smallest cost, closes the most-visible
   remaining orphan class. (~15 LOC)
2. **R4-M1** — three-line `audit_generation = $2` predicate on
   `set_backfill_running` + 0-rows-affected → `err_reset_externally`
   in exec_begin. Closes the last gap-X variant. (~3-5 LOC)
3. **R4-I1** — DDL row session/heartbeat columns + `tracing::warn!`
   on the five silent UPDATE discards + sweeper. Largest LOC delta
   but biggest operator-visibility win. (~30-50 LOC)

After (1) + (2) + extending the `tracing::warn!` part of (3) to all
five sites: score floor moves to ~87-88, with deductions concentrated
in R4-M2 (type-split for lenient) and R4-M3 (catch_unwind). Both are
next-stratum refactors.
