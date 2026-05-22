# plugin-db: Migration / DDL Pipeline Correctness Review (R3)
**HEAD:** 43ac6c8e · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72/100) · r2 (81/100, 00:47)

---

## 1. Pipeline diagram at HEAD (post-`cbd12944`)

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
            ┌── build_ctx:
            │     ensure_app_schema
            │     ensure_audit_table
            │     next_schema_version
            │     expand declared_indexes
            │   ──Err──► guard.release().await; return Err(e)   (bootstrap.rs:136-139)
            └── Ok → (RegisterContext, OrchestratorLockGuard<'p>)
               ▼
          compute_plan                            (plan.rs:34)
          ──Err──► run_pipeline catches at mod.rs:209-219
                   guard.release().await; return Err
               ▼
          validate                                (validate.rs:53)
            destructive + strictness != "off" → write Pending audit rows
            strict   → Err(envelope_json)
            lenient  → Ok(ApprovedPlan{ ops: plan.ops })          ← Pending rows ORPHAN (F2)
            off      → Ok(ApprovedPlan{ ops: plan.ops })
          strict Err → wrap as DbError::SchemaRefused (mod.rs:201-206)
                       guard.release().await; return Err (mod.rs:217)
               ▼
          apply                                   (apply.rs:37)
          ┌─ Pass 1 (under advisory lock — async block at apply.rs:201) ────┐
          │  for op in approved.ops:                                         │
          │    if op.class == Destructive → continue                         │
          │    if AddIndex → continue                                        │
          │    check_destructive_invariant(op)?       ← (3ef6a170, R1/C2)   │
          │    write_audit_row(Running)                                      │
          │    pool_exec(op.sql) / create_index_with_recovery for AddIndex   │
          │    update_audit_status(Applied|Failed)    ← errors `let _ =`d (F1)
          └──────────────────────────────────────────────────────────────────┘
          guard.release().await           (apply.rs:226 — always, before pass1?)
          pass1?                          (propagate after release)
          ┌─ Pass 2 (unlocked) ─────────────────────────────────────────────┐
          │  for op in approved.ops (AddIndex only):                        │
          │    check_destructive_invariant(op)? → write_audit_row(Running)  │
          │    create_index_with_recovery (CIC + audited retry loop)        │
          │    update_audit_status(Applied|Failed)    ← errors `let _ =`d   │
          └─────────────────────────────────────────────────────────────────┘

Backfill orchestrator (migrations.rs):
  exec_begin          → acquire_dedicated_client + try_advisory_lock(zs_mig:<app>, name)
                        ensure_audit_table (now map_audit_bootstrap_err — ed697c45)
                        insert_backfill_running OR set_backfill_running
                        capture start_generation
                        park client in ctx.mig_lock
       ▼
  exec_fetch_batch    → peek_latest_backfill_status; SELECT WHERE id > cursor LIMIT n;
                        heartbeat_backfill (best-effort)
       ▼
  exec_commit_batch   → BEGIN
                        lock_audit_row_for_update         ← serialises vs cancel/reset
                        if status='cancelled'     → ROLLBACK + err_cancelled_mid_run
                        if audit_generation drift → ROLLBACK + err_reset_externally
                        apply per-row UPDATEs
                        COMMIT/ROLLBACK                   ← row-lock released here
                        update_backfill_progress          ← runs UNLOCKED (F3 race — still open)
                        if is_done → finalise_backfill + release_advisory_lock + drop(client) + clear_mig_lock
```

### Audit-row state machines at HEAD

**DDL** (validate + apply):
```
                                ┌─── Pending (validate-refused destructive)  ← terminal in practice (F2 orphan)
                                │
write_audit_row ─────► Running ─┼── update_audit_status ──► Applied
                                │                       └─► Failed
                                │
                                └── (worker dies / update_audit_status error) ──► Running FOREVER (F1)
```

**Backfill** (migrations):
```
                                       ┌── progress (update_backfill_progress UNLOCKED) ──► Running
                                       ├── operator reset (audit_generation += 1) ──► Pending
INSERT phase='backfill' status='running' ─┤── operator cancel ──► Cancelled
                                       └── commit_batch(isDone) ──► Applied / AppliedWithDeadLetter / Failed / Cancelled
```

The `cbd12944` OrchestratorLockGuard refactor is **purely structural** — it centralises the explicit-unlock invariant that three prior commits (`b4e533e2`, `37a0ef76`, `3bb41fa1`) plugged inline at three pipeline stages. The semantic state machine is unchanged: bootstrap/plan/validate/apply still observe the same boundaries, the same hand-off (`bootstrap` returns the guard; `apply` consumes it), and the same release-on-every-error-path discipline. **No state-machine semantics changed; no audit-row transition logic changed.** F1, F2, F3, F4-F9 from R2 are therefore unchanged at HEAD by this commit.

---

## 2. Status of prior findings at HEAD

| Round | ID | Status at HEAD | Evidence |
|---|---|---|---|
| R1/C1 | replication schema/publication case mismatch | **FIXED** (r2) | `a00c41fd` — quote_ident used on schema reference |
| R1/C2 | `DropColumn`/`DropIndex` silent no-op | **FIXED** (r2) | `3ef6a170` — `check_destructive_invariant` at apply.rs:62; tests at apply.rs:325-395 |
| R1/M3 | bootstrap lock leak on ensure_*_table error | **FIXED** (r2) | `3bb41fa1` — now folded into guard.release() at bootstrap.rs:137 |
| R2/F1 | orphan `Running` DDL audit rows | **OPEN** | apply.rs:163-186 still uses `let _ = backend.update_audit_status(...).await;`; no sweeper for `phase='ddl' AND status='running'` |
| R2/F2 | orphan `Pending` audit rows from validate | **OPEN** | validate.rs:67-86 writes Pending; no terminal transition for `validation_refused` (status CHECK at audit.rs:229-231 doesn't include it) |
| R2/F3 | `update_backfill_progress` post-COMMIT, no generation predicate | **OPEN** | migrations.rs:577-598 still issues COMMIT then update_backfill_progress; audit.rs:725-754 has no `audit_generation` WHERE clause |
| R2/F4 | split lenient invariant (validate keeps destructive, apply skips) | **OPEN** | validate.rs:106; apply.rs:203, 237. `check_destructive_invariant` adds defence-in-depth but doesn't structurally fix |
| R2/F5 | no manual-approve / drift-detection path | **OPEN, scoped out** | A2 proposal scope |
| R2/F6 | CIC retry-exhaustion returns `cic_configuration` (cosmetic) | **OPEN** | backend/postgres.rs:595-602 — fallback Err arm; unreachable in current code |
| R2/F7 | concurrent register_model serialisation; Drop catastrophic path | **OPEN, documented** | lock_guard.rs:159-181 now codifies the Drop fallback explicitly |
| R2/F8 | exec_cancel doesn't release mig advisory lock; isolate wedged until teardown | **OPEN, documented** | migrations.rs:678-706 only updates audit row; v8_classes/migration.rs:94-131 unchanged |
| R2/F9 | `pg_index` validity check string-interpolates SQL escape | **OPEN, latent** | backend/postgres.rs:461-464 unchanged |

**Net at HEAD vs. R2:** OrchestratorLockGuard extraction (`cbd12944`) is the only register_model-pipeline structural change. Every R2 finding remains where it was. `ed697c45` + `c83d6a8c` are correctness improvements OUTSIDE the apply-side audit-row state machine (`.code` preservation at the migrations-orchestrator audit-bootstrap boundary, and an empty-RETURNING surfaced as `DbError::Internal` in replication) — they shore up the typed-error rail but don't move the audit-row-orphan needles.

---

## 3. R3 findings

### CRITICAL

**(none — the high-blast-radius R1 items remain fixed; R2's F1/F2/F3 are IMPORTANT-tier, not CRITICAL.)**

### IMPORTANT

**R3-I1 — Orphan `Running` DDL audit rows still unobservable, unsweepable.** (rolls forward R2/F1, no progress)

- **File:** `crates/plugin-db/src/orchestrator/register_model/apply.rs:163-188`; `crates/plugin-db/src/audit.rs:294-332` (DDL `write_audit_row` site)
- **Why:** Both terminal branches at apply.rs:163-186 end with `let _ = backend.update_audit_status(...).await;`. If the terminal UPDATE fails (transient pool error, statement timeout, connection drop, worker process kill between DDL success and update), the audit row is permanently `Running` — and there is no `owner_session_id` / `last_heartbeat_at` populated on the DDL write path (audit.rs:294-332 INSERTs only the ten columns; the session/heartbeat columns stay NULL). An operator scanning `__zeroship_migrations WHERE phase='ddl' AND status='running'` has no way to distinguish a genuinely in-flight DDL from an orphan: there's no session attribution and no liveness signal. The Backfill path uses `set_backfill_running` (audit.rs:539-562) to stamp `pg_backend_pid()` + `NOW()`; the DDL path does not.
- **Data loss / stall:** Per-deploy after a crash: at least one stale `Running` row that the next deploy's audit scan can't reconcile. Cross-deploy: no impact, because `OrchestratorLockGuard` still releases the lock on every observable path. The corruption is in *operator state observability*, not data state.
- **Compounding factor — Drop's catastrophic-path is now codified.** `OrchestratorLockGuard::drop` (lock_guard.rs:159-181) logs `tracing::error!` but cannot await. If a panic during `apply()`'s Pass 1 unwinds before `lock_guard.release().await` runs, the guard's Drop fires, the audit row stays `Running`, *and* the advisory lock stays held until the backend session closes. The combination doubles the operational signal (lock + orphan audit row) but neither is automatically recoverable. The Drop log message is a useful tripwire, but operators still need to manually reap.
- **Fix:**
  1. Populate `owner_session_id = pg_backend_pid()::text, last_heartbeat_at = NOW()` on the DDL INSERT. Two-line change in audit.rs:297-303 (column list + values).
  2. Replace `let _ = backend.update_audit_status(...).await;` with explicit `tracing::warn!` on Err so the silent discard is visible: `if let Err(e) = backend.update_audit_status(...).await { tracing::warn!(audit_id = id, error = ?e, "audit: terminal-transition update failed; row stays Running until sweeper reaps"); }`.
  3. Ship a sweeper that mirrors `replication::drop_abandoned_slots` (replication.rs:421) — scan `phase='ddl' AND status='running' AND (last_heartbeat_at IS NULL OR last_heartbeat_at < NOW() - INTERVAL '5 min')` and demote to `failed` with synthesised error. The bookkeeping invariant is the same as for backfill rows.
- **Verification:** apply.rs:163-186 (terminal `let _ =`); audit.rs:297-303 (DDL INSERT column list — no session/heartbeat); audit.rs:580-585 (backfill INSERT includes `owner_session_id, last_heartbeat_at`).

**R3-I2 — Orphan `Pending` audit rows from validate refusals.** (rolls forward R2/F2, no progress)

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:67-86`; `crates/plugin-db/src/audit.rs:229-231` (status CHECK)
- **Why:** validate.rs:67-86 writes a `Pending` row per destructive op when `strictness != "off"`. For `strict` validate returns Err and the pipeline aborts — the Pending row persists. For `lenient` validate returns Ok with the destructive ops still in `plan.ops`; apply skips them at apply.rs:203, 237 so `update_audit_status` is never called. Both modes orphan the Pending row.
- **Operator impact:** queries against `WHERE status='pending'` accumulate phantom rows forever — one per refused destructive op per deploy. A manual-approve / drift-detection UX is impossible without distinguishing "actionable pending" from "historical refusal."
- **Fix:**
  1. Add `validation_refused` to the `__zeroship_migrations_status_chk` CHECK constraint (audit.rs:229-231) and to `TerminalStatus` (audit.rs:148-166).
  2. At validate.rs:67-86, write the row with `InitialStatus::Running` then immediately follow with `update_audit_status(ValidationRefused, ...)` — mirrors the apply-layer state machine and avoids a new `InitialStatus` variant.
- **Verification:** validate.rs:74-85 (Pending write site); validate.rs:88-93 (strict returns Err, lenient falls through); apply.rs:203, 237 (destructive skip — no terminal transition); audit.rs:229-231 (CHECK constraint doesn't include `validation_refused`).

**R3-I3 — `update_backfill_progress` runs post-COMMIT and without `audit_generation` predicate — reset-clobber race window.** (rolls forward R2/F3, no progress)

- **File:** `crates/plugin-db/src/migrations.rs:575-598`; `crates/plugin-db/src/audit.rs:725-754`; `crates/plugin-db/src/audit.rs:627-649` (reset bumps generation)
- **Why:** migrations.rs:577 issues COMMIT — this releases the row lock acquired at `lock_audit_row_for_update` (line 487). migrations.rs:585-598 then calls `update_backfill_progress` *outside* the transaction. The UPDATE at audit.rs:725-754 writes `validate_cursor = $2, dead_letter_pks = $3, details.processed = $4` keyed **only** by `id` — no generation predicate.
- **Race window:** between COMMIT (line 577) and `update_backfill_progress` (line 585) an operator who calls `migrations.reset({name, collection})` runs `reset_backfill_row` (audit.rs:627-649) which sets `status='pending'`, `validate_cursor=NULL`, `dead_letter_pks=NULL`, `processed=0`, and bumps `audit_generation`. The subsequent `update_backfill_progress` then writes `validate_cursor = next_cursor` — silently clobbering the reset. The reset is silently lost; a fresh `exec_begin` reads the stale cursor and resumes from there instead of zero.
- **Why this isn't "user-responsibility":** R1 framed this as non-exactly-once after a crash (idempotency in user `migrateOne` is documented). The reset-clobber path is a *platform invariant break*: the user explicitly asked for "reset to zero" via `migrations.reset(...)` and the platform silently honoured neither the reset nor the commit's row-lock contract (the lock was released one statement too early).
- **Fix (pick one):**
  - **(a) preferred:** Include the progress UPDATE *before* the COMMIT statement. Same connection, same transaction, same row lock — eliminates the race entirely.
  - **(b) compatible:** Add `AND audit_generation = $5::bigint` to the WHERE clause of `update_backfill_progress` (audit.rs:740) and surface `RETURNING id` so the no-op case detects the reset and the SDK gets `migration_reset_externally` on the next batch. Mirrors the Gap-X guard at migrations.rs:506-509 but extended past the COMMIT boundary.
- **Verification:** migrations.rs:575-598; audit.rs:725-754 (no generation predicate); audit.rs:627-649 (reset bumps generation, clears cursor); migrations.rs:497-510 (existing Gap-X runs INSIDE BEGIN/COMMIT only).

### MINOR

**R3-M1 — Split lenient invariant remains: validate ships destructive ops in `ApprovedPlan`; apply's two `if op.class == Destructive { continue; }` lines are the only structural gate.** (R2/F4)

- **File:** `crates/plugin-db/src/orchestrator/register_model/validate.rs:106`; `apply.rs:203, 237`
- The R2 commit (`3ef6a170`) added `check_destructive_invariant` at apply.rs:62 as a *contract-violation tripwire* — it surfaces an `Internal` error if a `DropColumn`/`DropIndex` reaches `run_op` without the Destructive class. This protects against a regression in the diff classifier (the upstream side of the gate). It does **not** protect against a regression in `apply`'s two skip lines (the downstream side): if a future refactor accidentally removes the `if op.class == Destructive { continue; }` at apply.rs:203, a properly-classified `DropColumn` would slide through and the tripwire would still allow it (it checks the inverse condition — "is this a Drop op WITHOUT Destructive class").
- The structural fix is unchanged from R2: split the type — e.g. `enum ApprovedPlan { Strict(Vec<DiffOp>), LenientWithStripped(Vec<DiffOp>) }` where the lenient variant has destructive ops removed at the validate boundary. Apply then can't accidentally execute them because they're not in the data.
- The current shape is defence-in-depth at one layer, not two. Net classification: minor structural debt, low likelihood of regression in the near term.

**R3-M2 — `Drop` of `OrchestratorLockGuard` is documented but still strands the advisory lock on panic.** (R2/F7, now codified)

- **File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:159-181`
- The `cbd12944` refactor *codifies* the catastrophic-path fallback: panic during pipeline execution → guard's sync `Drop` runs → emits `tracing::error!` with `key` and `tag` fields → pooled connection returns to the pool with the session-scoped lock held → every subsequent caller blocks on `pg_advisory_lock(zs_reg:<app>, register_model)` until the backend session closes.
- The codification helps: operators now have a structured log event (`OrchestratorLockGuard dropped without release() or into_held()`) they can alert on. The fix proposed in R2/F7 — `std::panic::catch_unwind` around the spawned future at mod.rs:85-101 — is still unshipped.
- Recommendation: wrap the spawned future in `AssertUnwindSafe` + `catch_unwind`, convert a caught panic into `DbError::Internal { message: "register_model panicked: {panic_msg}" }` so the guard's `release()` runs on the resulting Err path. Net: the lock leaks only if the panic happens *inside* `release()` itself (the unlock SQL is best-effort already, so this is much narrower).

**R3-M3 — `pg_index` validity check still string-interpolates an escaped index name.** (R2/F9, unchanged)

- **File:** `crates/plugin-db/src/backend/postgres.rs:461-464`
- Same site as R1/I5 and R2/F9. With `standard_conforming_strings = on` (default on all supported PG versions) the `replace('\'', "''")` is correct. With it OFF, a `\` in the index name could shift the escape. Index names come through `query::build_named_indexes` / `query::build_create_indexes`, both of which restrict the character set, so this is a latent issue not a current bug. The fix is to parameterise the regclass cast: `query_text_params("SELECT indisvalid FROM pg_index WHERE indexrelid = $1::regclass", &[qualified_idx.as_str()])` — works fine because `regclass` accepts a string literal.

**R3-M4 — Migration v8_class `Drop` cancels the audit row but leaks the advisory lock until isolate teardown.** (R2/F8, unchanged)

- **File:** `crates/plugin-db/src/v8_classes/migration.rs:84-131`; `crates/plugin-db/src/migrations.rs:678-706`
- `Migration::drop` spawns `exec_cancel` which only flips the audit row to `cancelled` via `cancel_backfill_row_pool` (audit.rs:792-…). It does NOT call `release_active_lock`, does NOT clear `ctx.mig_lock`, and does NOT drop the parked lock client. The lock therefore stays held until the isolate dies (which releases the connection task → backend session ends → lock auto-released).
- Subsequent `exec_begin` on the same isolate hits `migration_already_active` at migrations.rs:233-240 because the `mig_lock` slot is still populated. The isolate is wedged for that migration name for the lifetime of the isolate.
- Documented at v8_classes/migration.rs:106-108. Behaviour, not bug — but it's the documented version of a real foot-gun: a mis-cancelled run can wedge an isolate indefinitely. A follow-up could extend `exec_cancel` to optionally accept the parked lock client + release it; v8_classes/migration.rs's Drop would then thread `take_lock_client()` through to the spawned cancel. This is a non-trivial restructuring (the lock client lives in `ctx.mig_lock`, which is `RefCell`-scoped to the isolate's compio context — the spawned cancel is on the same runtime, so it's tractable).

**R3-M5 — CIC retry-exhaustion fallback returns `cic_configuration`.** (R2/F6, cosmetic, unchanged)

- **File:** `crates/plugin-db/src/backend/postgres.rs:595-602`
- Same site as R2/F6. The loop at 456-588 always returns from inside, so the fallback is unreachable in current code. The variant choice (`Configuration` vs `SchemaRefused { code: "cic_failed" }`) is the cosmetic complaint — every other terminal arm in the function returns `SchemaRefused`, so the fallback being `Configuration` would surface in operator dashboards as a config issue rather than a retry-exhaustion. Defence-in-depth tripwire; low priority.

---

## 4. Stage invariants at HEAD

| Stage | Assumes | Guarantees | Failure mode |
|---|---|---|---|
| **bootstrap** | Pool initialised | `OrchestratorLockGuard` held; schema + audit table exist; `schema_version` monotonic; indexes expanded | Err → `guard.release().await; return Err` (bootstrap.rs:136-139) |
| **plan** | Lock held (via guard) | `Vec<DiffOp>` classified | Err → caller (mod.rs:209-219) `guard.release().await; return Err` |
| **validate** | Lock held | `ApprovedPlan` retains destructive ops (lenient/off) or short-circuits (strict). **Pending audit rows for refused destructive ops are NOT terminalised** (R3-I2) | strict Err → wrap `SchemaRefused`; caller releases |
| **apply pass 1** | Lock held | Non-destructive non-index ops run; audit rows written; `check_destructive_invariant` traps misclassified Drops; **terminal-transition errors silently discarded** (R3-I1) | Err → release lock, then `pass1?` propagates (apply.rs:226-229) |
| **apply pass 2** | Lock released | CIC ops run idempotently; retry loop with `pg_index` indisvalid check + audited retry log | terminal → `SchemaRefused { code: "cic_failed" }`; unreachable fallback → `cic_configuration` (R3-M5) |
| **exec_begin** | No active migration on isolate | Session-scoped advisory lock on dedicated `Client`; audit row in `running`; `start_generation` captured; **`map_audit_bootstrap_err` preserves typed `.code`** (`ed697c45`) | Err → client dropped (lock auto-releases) |
| **exec_commit_batch** | Advisory lock held; BEGIN open | Audit row locked FOR UPDATE; generation check inside tx; per-row UPDATEs; COMMIT/ROLLBACK. **`update_backfill_progress` runs UNLOCKED** (R3-I3) | dry-run → ROLLBACK; real run → COMMIT + progress write; is_done → finalise + release + drop |
| **exec_cancel** (operator) | Pool client | Audit row → `cancelled`. **Does NOT release the in-flight worker's advisory lock or parked lock client** (R3-M4) | benign for the audit row; isolate wedged for migration name until teardown |
| **exec_reset** (operator) | Pool client | `status='pending'`, cursor=0, processed=0, `audit_generation += 1` | Gap-X guards in-flight commits — except for the R3-I3 post-COMMIT window |
| **OrchestratorLockGuard::release** | Guard live | Best-effort `pg_advisory_unlock`; returns unlocked PooledClient | SQL errors swallowed (lock_guard.rs:126-128) — relies on session close for catastrophic fallback |
| **OrchestratorLockGuard::drop** (panic path) | n/a | Logs `tracing::error!`; PooledClient returns to pool with session lock held | Documented catastrophic fallback (lock_guard.rs:159-181) |

---

## 5. Concurrent `register_model` (re-walked under guard refactor)

The R2 audit confirmed correct serialisation via `hashtext('zs_reg:<app>')` + `hashtext('register_model')`. Re-walking under `cbd12944`:

- **Acquisition site:** lock_guard.rs:87-100 — `acquire` calls `backend.acquire_advisory_lock` on the pooled client. If it fails, `Err` returns and the client drops (no lock held, returns to pool).
- **Bootstrap Err path:** bootstrap.rs:136-140 — `guard.release().await` BEFORE return Err. Unlock + client returned to pool. ✓
- **Plan/validate Err path:** mod.rs:209-219 — same. ✓
- **Apply Pass 1 Err path:** apply.rs:201-229 — async block captures Result; `guard.release()` always runs (line 226), THEN `pass1?` (line 229). ✓
- **Apply Pass 2 Err path:** apply.rs:232-240 — runs unlocked. ✓ Any Err propagates without lock concerns.
- **Validate strict refusal path:** mod.rs:201-206 wraps envelope as `DbError::SchemaRefused`; mod.rs:217 releases guard. ✓
- **Panic in any stage:** guard's sync `Drop` logs error; PooledClient returns to pool with session lock held. Lock auto-releases when the backend session ends (pool recycle / connection close). Documented; not automatically reapable.

**Net:** every observable error path through the four-stage pipeline now flows through a single guard-release call. The R2 verdict — "no outstanding lock-leak class in the four-stage pipeline" — holds. The cbd12944 refactor moved the invariant from three open-coded sites to one centralised one, reducing the surface for future contributors to forget the release call. **No regression introduced; no new defect surfaced.**

The one remaining lock-leak class is the panic-unwind path (R3-M2); fixable with `catch_unwind` around the spawned future at mod.rs:85-101.

---

## 6. Audit-row terminal-transition reliability

Three sites discard `update_audit_status` errors via `let _`:

1. **apply.rs:163-170** — DDL Applied transition (Pass 1 + Pass 2).
2. **apply.rs:178-185** — DDL Failed transition.
3. **backend/postgres.rs:480-487, 522-529, 561-568** — CIC retry loop audit transitions (three sites: invalid_index_landed, data_violation, transient_retry).

Across all five sites, the pattern is identical: terminal-update errors are silently swallowed. The `let _` is consistent with the function returning `Result<bool, DbError>` (the bool indicates whether the UPDATE matched any rows) — but neither the bool nor the error is consumed.

**Reliability impact:** if the terminal-transition UPDATE fails (transient pool error, statement timeout, connection drop), the audit row stays `Running`. The DDL itself may have succeeded — the audit row becomes a false negative for "what's still in flight?" queries. With no `owner_session_id` / `last_heartbeat_at` on DDL rows, the operator cannot distinguish orphan from genuine.

This is R3-I1. The fix is two-part:
1. **Observability:** replace `let _` with `tracing::warn!` so the silent discard becomes visible.
2. **Reapability:** populate `owner_session_id` + `last_heartbeat_at` at DDL row INSERT; ship a sweeper that demotes stale `Running` DDL rows to `Failed`.

The OrchestratorLockGuard refactor (`cbd12944`) is NOT relevant here — these are audit-row writes through the pool, not through the lock client. The guard only protects against advisory-lock leaks, not audit-row state leaks.

---

## 7. Score

**81 / 100** (unchanged from R2)

### Movement

- **+0 net** since R2.
- **Structural improvement landed:** `cbd12944` (OrchestratorLockGuard RAII) centralises the explicit-unlock invariant. This *reduces* future regression surface — a contributor adding a new pipeline stage or a new error path now wires through the guard rather than open-coding the unlock SQL. Worth +1 in defence-in-depth terms.
- **Audit-row hygiene unchanged:** F1, F2, F3 are still open, in the exact same shape. Worth -1 for the missed opportunity (one of these three could likely have shipped alongside the guard refactor).
- **Adjacent improvements landed:** `ed697c45` (`map_audit_bootstrap_err`) restores typed-error `.code` discipline at four migrations-orchestrator audit-write sites; `c83d6a8c` plugs an empty-RETURNING silent-default in replication. Both are correctness wins outside the apply-side state machine. Net zero on this lens; positive on the typed-error rail and the slot-management lens.

### Score components

- **Strict-deploy correctness:** strong. Plan/validate/apply boundaries crisp; lock release on every observable Err path. (90/100 contribution)
- **Lenient/off-deploy correctness:** weaker — split invariant remains (R3-M1); destructive-skip is two lines, not a type-level guarantee. (75/100)
- **Audit-row state machine consistency:** weakest — three orphan classes (R3-I1 DDL Running; R3-I2 Pending; R3-I3 reset clobber). Each is narrow but cumulative; the platform's "drift detection / deploy reconciliation" UX is blocked on these. (60/100)
- **Backfill orchestrator:** improved at the audit-bootstrap boundary by `ed697c45`. The exec_commit_batch row-lock + generation guard is sound INSIDE the transaction; the gap is the unlocked update_backfill_progress at the tail. (75/100)
- **CIC recovery loop:** robust. The retry budget logic is sound; the fallback variant choice (R3-M5) is cosmetic; the `pg_index` interpolation (R3-M3) is latent. (90/100)
- **Concurrent register_model:** improved by the guard refactor (centralised invariant). Only catastrophic-path leak remains (panic-unwind). (88/100)

### Comparison to R2 (81)

The pipeline structure is **measurably more resilient** under `cbd12944` — three open-coded unlock sites collapsed into one type-enforced guard. That's an improvement in *future regression resistance* rather than current correctness, so it doesn't move the headline score. The three open audit-row findings (F1/F2/F3 → R3-I1/I2/I3) are unchanged in shape and severity: they accumulate state pollution over operational time rather than corrupt data per deploy.

**Trajectory:** positive on the safety net (guard); flat on audit-row hygiene; slightly positive on adjacent typed-error rails (`ed697c45`, `c83d6a8c`). The next round-over-round improvement requires shipping at least one of R3-I1 / R3-I2 / R3-I3 — they're the three remaining state-machine-hygiene defects with operator-visible consequences.

Suggested order of attack (smallest cost, highest leverage):
1. **R3-I3 (a)** — move `update_backfill_progress` BEFORE the COMMIT. Two-line move in migrations.rs:575-598. Closes the reset-clobber race window entirely.
2. **R3-I1 (steps 1+2)** — add `owner_session_id` + `last_heartbeat_at` to the DDL INSERT; replace `let _` with `tracing::warn!`. Five-line change in apply.rs + audit.rs.
3. **R3-I2** — extend `TerminalStatus` with `ValidationRefused`; update the CHECK constraint; flip the validate.rs Pending writes to Running + immediate-Refused. ~15 lines across audit.rs + validate.rs.

After those three: the score floor moves to ~88, with the remaining deductions concentrated in R3-M1 (structural type-split for lenient mode) and R3-M2 (catch_unwind around the spawned future). Both are next-stratum refactors.
