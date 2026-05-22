# plugin-db: Migration / DDL Pipeline Correctness Review (R11)

**HEAD:** `71a457a1` · **Date:** 2026-05-22 · **Reviewer:** fresh lens
**Prior rounds:** r1 (72) · r2 (81) · r3 (81) · r4 (82) · r5 (83) · r6 (84) · r7 (85) · r8 (85) · r9 (85) · r10 (85)

**Forcing function:** r10 recommended re-firing this lens once F1's
warn-half landed. It did at `fcf7ce3c plugin-db: warn on audit-status
update failure (F1 warn-half)`. This round audits the warn-half for
completeness, consistency, regressions, plus the new
`release_advisory_lock` typed-Result (`51c342e8`, [I6]).

---

## 0. Pipeline delta since r10 (`2d34061e` → `71a457a1`)

| Commit | Module | Pipeline relevance |
|---|---|---|
| `51c342e8` | `backend/{mod,postgres}.rs`, `migrations.rs` | [I6] `release_advisory_lock` returns `Result`; two `migrations.rs` callers (exec_begin cancelled-refusal; exec_commit_batch finalise) `warn!` on Err. **Directly pipeline-relevant.** |
| `fcf7ce3c` | `backend/postgres.rs`, `register_model/apply.rs` | **F1 warn-half.** All 5 `let _ = update_audit_status(...).await;` sites converted to `if let Err(e) = ... { tracing::warn!(...); }`. |
| `71a457a1` | `auth/mod.rs`, `lib.rs` | Pure doc-drift after `2fa9472e` Cargo gate. Orthogonal. |

Verified no other migration-pipeline code change in window:

```
$ git diff 2d34061e..71a457a1 -- \
    crates/plugin-db/src/audit.rs \
    crates/plugin-db/src/orchestrator/register_model/validate.rs
(empty)
```

`audit.rs` (status CHECK, `write_audit_row`, backfill setter) and
`validate.rs` (Pending write rail) are byte-identical to r10.

---

## 1. F1 warn-half audit (the forcing function)

### 1.1 All 5 r10-enumerated sites converted?

Yes, byte-verified at HEAD:

| # | File:line @ HEAD | Status | Fields carried |
|---|---|---|---|
| 1 | `apply.rs:163-182` | converted | `app_id` (`%`), `audit_id`, `error` (`?`) |
| 2 | `apply.rs:190-210` | converted | `app_id` (`%`), `audit_id`, `ddl_error` (`%`), `audit_error` (`?`) |
| 3 | `postgres.rs:487-507` | converted | `app_id`, `audit_id`, `attempt`, `error` (`?`) |
| 4 | `postgres.rs:542-562` | converted | `app_id`, `audit_id`, `sqlstate`, `error` (`?`) |
| 5 | `postgres.rs:594-614` | converted | `app_id`, `audit_id`, `attempt`, `transient`, `error` (`?`) |

`git grep "let _ = .*update_audit_status"` returns zero hits in
`crates/plugin-db/src/`. **The set is closed.**

### 1.2 Warn-shape consistency

Each warn carries `app_id` + `audit_id` + inner error. Site-specific
fields vary (`attempt`, `sqlstate`, `transient`, `ddl_error`) — each
records the *available* context at its call-site, which is the right
choice (the retry-loop sites have `attempt`; the apply.rs sites
don't, because apply has no retry counter). Consistency across the
five sites is structurally sound.

Minor cosmetic inconsistency: `apply.rs` uses `app_id = %app_id`
(typed_id → Display) while `postgres.rs` uses bare `app_id,` (the
local is `&str`). Both render identically in JSON tracing output —
acceptable. Not a code smell.

The `error` field uses `?e` (Debug) in 4 sites and `%msg` (Display)
+ `?upd_err` (Debug) for the audit-error in site 2 (apply.rs Failed
arm). Mixed but defensible: Debug surfaces `DbError` variant + inner
chain; Display gives the human-readable DDL error message that
already went out to the caller. No regression.

### 1.3 Regression check: retry-loop semantics, shadowed bindings, `?`-propagation

- **Retry-loop semantics:** unchanged. The `if let Err(e) = ...
  { warn!(...); }` block has no return / continue / break inside it;
  control flows through to the subsequent `let _ = pool.query_text_params(&drop_idx_sql, ...).await;`
  and the `return Err(refuse(...))` / `attempt == MAX_RETRIES` check
  exactly as before. Byte-verified in the diff.
- **Shadowed bindings:** site 2 (data-violation) uses `upd_err` as
  the bound name to avoid shadowing the outer `e` (the SQL error
  from the index build). Site 5 (transient/non-transient) likewise
  binds `upd_err`. Site 4 (apply.rs Failed) binds `upd_err` for the
  same reason. Sites 1 + 3 (apply.rs Applied; postgres.rs INVALID
  index) bind `e` since no outer `e` is in scope. Hygienic.
- **`?`-propagation:** none of the new `if let Err(e) = ...await { ... }`
  blocks suppress a `?` that would have propagated before — the
  prior code was `let _ = ...await;` which already discarded the
  Err. Net behaviour: previously the audit-write Err was silently
  dropped; now it's logged. The retry-loop's outer `return Err(refuse(...))`
  still fires from the surrounding code, not from inside the warn block.

**Verdict on warn-half:** clean. Five sites converted, consistent
shape, no semantic regression, no shadow / `?` hazards. The commit
title says "warn-half" and that is precisely and only what landed.

### 1.4 Other `let _ = ...await` patterns in the migration pipeline?

`git grep "let _ = .*\.await" crates/plugin-db/src/` returns 12 sites.
Classification:

| Site | Best-effort? | Status |
|---|---|---|
| `migrations.rs:476` (`ROLLBACK` on rollback-and-return) | yes — cleanup SQL on a tx that's already doomed | ok |
| `lib.rs:256` (test-helper `ROLLBACK;pg_advisory_unlock_all`) | test-only | ok |
| `lib.rs:279` (test-helper `connection.run()`) | test-only spawn | ok |
| `lib.rs:286` (test-helper `BEGIN`) | test-only | ok |
| `lib.rs:315` (test-helper `ROLLBACK`) | test-only | ok |
| `postgres.rs:509` (drop_idx_sql after INVALID-index audit) | best-effort drop | ok |
| `postgres.rs:564` (drop_idx_sql after data-violation audit) | best-effort drop | ok |
| `postgres.rs:617` (drop_idx_sql after transient/non-transient audit) | best-effort drop | ok |
| `lock_guard.rs:85` (docstring example) | n/a | doc |
| `bootstrap.rs:137` (`guard.release()` on bootstrap-fail) | `release()` already warns internally at `lock_guard.rs:172-179` | ok |
| `register_model/mod.rs:219` (`lock_guard.release()` on early-fail) | same — release() already warns | ok |
| `apply.rs:251` (`lock_guard.release()` on pass-1 completion) | same — release() already warns | ok |

None are audit-row state-machine writes. The three `drop_idx_sql`
calls are arguably worth a `warn!` (a stuck DROP INDEX leaves an
INVALID index name occupied for the next CIC retry to collide on),
but that's a **new** observability gap, not a regression and not
on the F1 list. Out of scope for r11.

The three `lock_guard.release()` `let _ =` sites are fine: the
`release()` Result-tail discards a `Result<Option<PooledClient>, _>`
where the Err arm already emitted a `tracing::warn!` from inside
the guard at `lock_guard.rs:172-179`. Double-logging avoided.

---

## 2. `release_advisory_lock` → Result (51c342e8)

### 2.1 Trait + impl

`backend/mod.rs:148-164` — signature changes from `async fn ... -> ()`
to `async fn ... -> Result<(), DbError>`. Doc rewritten to say "the
lock auto-releases on session end, so callers can treat an `Err` as
observability-only (warn-and-continue)".

`backend/postgres.rs` impl lifts `compio_postgres::Error` via
`DbError::from_pg` (verified in commit diff, file too long to
re-paste here). Same idiom used elsewhere — no novel error-mapping
risk.

### 2.2 Caller 1 — `migrations.rs:284-300` (exec_begin cancelled-refusal)

```rust
if row.status == "cancelled" {
    if let Err(e) = backend
        .release_advisory_lock(&client, &lock_key, name)
        .await
    {
        tracing::warn!(
            app_id, name, error = %e,
            "release_advisory_lock failed on cancelled-refusal path \
             (lock auto-releases on session end)",
        );
    }
    drop(client);
    return Err(err_cancelled_on_start());
}
```

Pre-change: `backend.release_advisory_lock(...).await;` (no `let _`,
because the prior signature was `()`). Post-change: `if let Err`
+ `warn!`. Control flow identical (no early-return inside the if-Err;
the `drop(client)` and `Err(err_cancelled_on_start())` still fire).

### 2.3 Caller 2 — `migrations.rs:658-669` (exec_commit_batch finalise)

```rust
if let Err(e) = backend
    .release_advisory_lock(&client, &lock_key, &name)
    .await
{
    tracing::warn!(
        app_id, name, error = %e,
        "release_advisory_lock failed on backfill-finalise path \
         (lock auto-releases on session end)",
    );
}
drop(client);
crate::context::with_mut(|c| c.clear_mig_lock());
return Ok(serde_json::json!({ "committed": !dry_run, "done": true }).to_string());
```

Same shape. The `clear_mig_lock` + `return Ok(...)` still fires
after the warn block. The success path still returns success even
if the unlock SQL failed — defensible, because the lock auto-releases
when the session ends; the warn is the diagnostic.

### 2.4 Does it help diagnose stuck migrations?

Yes, materially. Before `51c342e8`, an unlock-SQL failure on the
*backfill-finalise* path meant the audit row got flipped to
`applied` / `applied_with_dead_letter` but the session-scoped
advisory lock stayed held for the pool-recycle interval. Operators
would observe a *new* `exec_begin` on the same `(app, name)` block
on the advisory lock with **no log line** explaining why — they'd
look at the audit row (`applied`) and conclude the prior run was
clean, then chase the lock leak by hand.

With the typed Result + warn, the post-mortem signal exists. **The
diagnostic gap on the finalise path is closed.** This complements
the `5d9acab8` `set_mig_lock` / `return_mig_client` tracing pair
landed in r10 — together they make the full "stuck migration"
diagnostic surface observable.

**Verdict on `51c342e8`:** unambiguous positive observability
change. Same correctness model (lock auto-releases on session end,
warn-and-continue) — but now the warn fires. +1 to "concurrent
register_model" component (was 90, now 91) on the strength of the
finalise-path diagnostic.

---

## 3. Re-check audit-row state machine for new drift since r10

`audit.rs` unchanged — status CHECK still:

```sql
status IN ('pending','running','applied','applied_with_dead_letter',
           'failed','cancelled','rolled_back')
```

`validate.rs` unchanged — strict still short-circuits at line 90-92
with Pending row left behind; lenient still falls through with the
destructive op retained in `plan.ops` (skipped by apply at lines
203/233). **No drift.** F2 status unchanged byte-for-byte from r9.

`audit.rs:283-316` write_audit_row INSERT — still omits
`owner_session_id` and `last_heartbeat_at`. Backfill rail still
stamps `pg_backend_pid()::text + NOW()`. The asymmetry F1's sweeper-
half needs to resolve (DDL rows need an ownership column + heartbeat
to be sweep-claimable) is unchanged.

---

## 4. F1 sweeper-half — landscape re-statement

The warn-half does **observability only**. The sweeper-half — the
larger fix — still needs:

1. **Schema migration** to add `owner_session_id TEXT` +
   `last_heartbeat_at TIMESTAMPTZ` to DDL audit rows (currently
   only backfill rows stamp these).
2. **Stamping at write time** in `audit.rs:283-316` (write_audit_row)
   so DDL rows record `pg_backend_pid()::text + NOW()` like backfill
   rows do at `audit.rs:525-545`.
3. **Heartbeat cadence** during the DDL pass — currently a single
   `Running` flip happens at the start, with no liveness updates.
   Options:
   - (a) Heartbeat every N seconds during long DDLs (CIC retry
     loops can run minutes).
   - (b) Skip heartbeat; rely on session-id + `pg_stat_activity`
     join at sweeper time to decide liveness. Less invasive.
4. **Sweeper itself** — a background task per worker that periodically
   queries `audit_table WHERE status='running' AND (last_heartbeat_at
   < NOW() - INTERVAL '<grace>' OR NOT EXISTS (SELECT 1 FROM
   pg_stat_activity WHERE backend_pid::text = owner_session_id))`
   and flips them to `failed` with reason `sweeper_reclaimed`.
5. **Cadence + grace policy** — where does the sweeper live? Per
   worker? Per app? Cron? On every `register_model` start? r9-r10
   left this as design-open.

Landscape unchanged since r9. No commits in the window changed
either the schema shape, the heartbeat surface, or any sweeper
scaffolding. Design space identical to r10's enumeration.

**One-liner status:** F1 sweeper-half **still open**; warn-half
**closed at `fcf7ce3c`**.

---

## 5. F2 status

`audit.rs:218-220` status CHECK unchanged. `validate.rs:66-95`
unchanged. **One-liner status:** F2 **still open**, byte-identical
to r10.

---

## 6. Score

**86 / 100** (vs r10's **85**: **Δ +1**)

### Score components vs r10

- **Strict-deploy correctness:** 91/100 (unchanged)
- **Lenient/off-deploy correctness:** 75/100 (unchanged)
- **Audit-row state machine consistency:** **70/100** (was 68; +2
  from F1 warn-half — the silent-swallow class of audit-write
  secondary failures is now observable. Schema asymmetry + missing
  `validation_refused` still cap the component.)
- **Backfill orchestrator:** 82/100 (unchanged)
- **CIC recovery loop:** **91/100** (was 90; +1 from F1 warn-half
  surfacing the three CIC-loop audit-write secondary failures and
  from `51c342e8` typed Result on the backfill-finalise unlock).
- **Concurrent `register_model`:** **91/100** (was 90; +1 from
  `51c342e8` closing the finalise-path advisory-lock diagnostic gap).
- **Error-helper consistency / dedup:** 90/100 (unchanged)

Aggregate moves +1 from 85 → 86. Three of seven components budged
upward; none regressed.

### Round-over-round signal

- r5 → r6: +1
- r6 → r7: +1
- r7 → r8: +0
- r8 → r9: +0
- r9 → r10: +0
- **r10 → r11: +1** (forcing function fired)

The plateau broke. **The warn-half forcing function moved the
score.** The +1 is exactly what r10 projected ("Moves lens floor
1-2 points") for the F1 warn-half close, and the `51c342e8` typed-
Result close reinforced the same observability axis.

---

## 7. Did the warn-half forcing function move the score, or are we still on plateau?

**Moved.** +1 (85 → 86). Plateau broken. The next ceiling is the
F1 sweeper-half (schema + stamping + watchdog) which would move
the audit-row state machine consistency component from 70 → ~80
and the aggregate to ~88-89. F2 lands `validation_refused` and
moves lenient/off correctness from 75 → ~83 and aggregate to
~89-90. Both remain open.
