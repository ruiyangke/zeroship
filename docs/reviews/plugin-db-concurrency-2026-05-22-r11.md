# plugin-db concurrency / lifecycle review — 2026-05-22 r11

**Commit:** `81226451` (HEAD; post-r10 cycle 12:47)
**Lens:** concurrency + lifecycle (round 11, forcing-function re-walk)
**Prior rounds:** `…-r10.md` (88), `…-r9.md` (88), `…-r8.md` (88),
`…-r7.md` (86), `…-r6.md` (84), `…-r5.md` (80), `…-r4.md` (80),
`…-r3.md` (73), `…-r2.md` (82).

**Forcing-function commits this cycle:**

- `51c342e8` — `release_advisory_lock` returns `Result<(), DbError>`
  (I6). Trait sig + Postgres impl + two `migrations.rs` callers
  (cancelled-refusal at `:287-297`; backfill-finalise at `:661-671`).
- `fcf7ce3c` — F1 warn-half. Five `let _ = update_audit_status(...)`
  sites converted to `if let Err(_) = ...` + structured
  `tracing::warn!` (apply.rs Ok + Err branches; three CIC-recovery
  sites in `backend/postgres.rs`).
- `7c6bd2ec` — F1 warn-shape unification across the 5 fcf7ce3c sites +
  `finalise_backfill` warn (`migrations.rs:647-657`); added
  `name`/`collection` to the latter.

**Adjacent (non-sync-surface) commits in window**: `71a457a1`
(doc-drift), `ae5570dc` (exec.rs unit tests +133 LOC, production code
untouched), `4cab871a` (3 doc/visibility), `3b9b458d` (deferred docs),
`403b3891` (query.rs validate_field_name non-ASCII), `05484878`
(reviews), `5d9acab8` (context.rs `set_mig_lock`/`return_mig_client`
tracing surface; semantic state unchanged), `2fa9472e` (auth subtree
hardening gate).

---

## Headline

**The forcing-function commits do NOT move the headline.** I6
(release_advisory_lock returns Result) and F1 warn-half (audit-status
warn-instead-of-swallow) are both **observability-only** changes on
the audit/lock state machines. They neither introduce nor close a
synchronisation hazard:

- **2b is still open**, semantically identical to r10. The widened
  text in the is_done branch (was `:651→:654`, now `:661→:674`, 13
  lines) introduces **zero new async points** between the
  `release_advisory_lock.await` resolution and `clear_mig_lock()`.
  The `tracing::warn!` macro is synchronous; `drop(client)` is
  synchronous. The cancellation window in async-cancellation terms is
  unchanged.
- **2a is byte-identical** to r10's reading (`exec.rs:43-73`,
  `transaction.rs:119-141`, `:240-246`, `:266-270`).
- **2c (apply.rs MINOR) IS CLOSED** by `fcf7ce3c` + `7c6bd2ec`. Both
  Ok and Err branches now emit structured warns with unified field
  shape (`app_id = %app_id`, `audit_id`, `transition`,
  `audit_err = %audit_err`).
- broker.rs not touched. `:319-321`, `:325-327`, `:359-369` all
  byte-identical. PENDING_EMITS path not touched. WAL consumer not
  touched. TX_CONN ownership sites not touched. Cross-isolate
  surface not touched.

Score **89 / 100 (+1 vs r10)**. The +1 is the closure of the apply.rs
MINOR carry; no new findings; the two IMPORTANTs persist verbatim.
The forcing function moved the floor by exactly the lift predicted in
r10 (apply.rs MINOR = +1) — confirming r10's plateau math.

---

## 2b — re-walked under the new typed-error path

**File:** `migrations.rs:614-679` at HEAD.

The new is_done branch:

```rust
if is_done {
    let terminal = match terminal_status.unwrap_or("applied") { ... };

    if let Err(e) = backend                                  // :643
        .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
        .await
    {
        tracing::warn!(                                      // :647-657
            app_id = %app_id, name = %name, collection = %collection,
            audit_id = audit_id, terminal = ?terminal, error = %e,
            "finalise_backfill failed; ..."
        );
    }

    let lock_key = format!("zs_mig:{app_id}");
    if let Err(e) = backend                                  // :661
        .release_advisory_lock(&client, &lock_key, &name)
        .await
    {
        tracing::warn!(                                      // :665-670
            app_id, name, error = %e,
            "release_advisory_lock failed on backfill-finalise path \
             (lock auto-releases on session end)",
        );
    }
    drop(client);                                            // :673
    crate::context::with_mut(|c| c.clear_mig_lock());        // :674
    return Ok(...);
}
```

**Race-window analysis under the brief's question:**

The brief asks: *"if `release_advisory_lock` returns Err, we now
`tracing::warn!` and proceed — does proceeding-after-Err change
whether `mig_lock` is correctly cleared?"*

**Answer: no, and the cancellation window is no wider.**

1. **Err path (non-cancelled):** if the `pg_advisory_unlock` SQL
   itself errors (e.g., session lost mid-await), control reaches the
   warn arm `:665-670`, falls through to `drop(client)` at `:673`,
   then `clear_mig_lock()` at `:674`. The in-process slot IS cleared.
   This is functionally identical to the Ok path. The lock-release
   server-side is irrelevant for in-process state (PG auto-releases
   on session close, which `drop(client)` triggers). **State is
   consistent.**

2. **Cancellation path (await did not resolve):** if the JS Promise
   is cancelled while suspended on `:663`'s await, the future unwinds
   without reaching `:673`/`:674`. The `client` is dropped via the
   unwind (session closes → PG releases server-side lock). The
   in-process `mig_lock` slot is **NOT** cleared. Same hazard as
   r10's 2b.

3. **Cancellation between await-resolve and clear_mig_lock:** would
   require an await point between `:663` and `:674`. There are
   **none**. The `tracing::warn!` expansion is sync (the `tracing`
   crate's macros do not introduce awaits). `drop(client)` is sync.
   `with_mut` is sync. The cancellation surface is exactly the
   await on `release_advisory_lock` itself — unchanged from r10.

**Verdict:** the I6 typed-error path makes the **non-cancel Err case
strictly safer for observability** (we now log instead of swallow),
but does **not** change the cancel-during-await race window. r10's
2b carry persists **verbatim** as a fix-target.

The fix recommendation also stands unchanged: a `MigLockGuard` RAII
type whose `Drop` calls `clear_mig_lock` regardless of where the
future unwound. Constructed at `migrations.rs:328-336`; released by
either falling out of scope (cancel/panic) or via an explicit
`mem::forget` / `into_persistent` if the caller hands ownership
elsewhere.

**Cross-check on the other release_advisory_lock site at
`migrations.rs:287-297`** (cancelled-refusal path): re-confirmed safe
under the I6 typed-error path. `set_mig_lock` is called later at
`:329`. At the point of the `.await` at `:289`, the in-process slot
has never been set; cancellation between `.await` and `drop(client)`
relies on PG session-close auto-release. The new typed-error path
just adds a warn arm at `:291-296` — synchronous, no new race.
**Status: safe, unchanged.**

---

## 2a — byte-identical at HEAD

Re-read `exec.rs:43-73`, `v8_classes/transaction.rs:119-141`,
`:224-273`. Every line matches r10's recorded body. `run_sql` still
takes `tx_client` across `.await` at `:53`; `Transaction::drop` still
short-circuits on `settled.get()` at `:121-122` before reaching
`clear_pending_emits()` at `:139`; `end()`'s `client_opt = None`
early-out at `:240-246` still sets `settled = true` + returns Ok
without clearing pending_emits.

**Status: carry-over IMPORTANT, unchanged.**

Fix family also unchanged (one-liner each site):
- `Transaction::drop` settled-true early-out at `:121-122`: call
  `crate::exec::clear_pending_emits()` before returning.
- `Transaction::end` client-already-cleared early-out at `:240-246`:
  same.

---

## 2c — apply.rs MINOR is CLOSED

**File:** `orchestrator/register_model/apply.rs:160-220` at HEAD.

Both branches now warn structurally (verified line-by-line):

```rust
// Ok branch (:163-185) — verified verbatim
if let Err(audit_err) = backend.update_audit_status(...).await {
    tracing::warn!(
        app_id = %app_id, audit_id = id, transition = "Applied",
        audit_err = %audit_err,
        "update_audit_status failed; row stays in 'running' until reset",
    );
}

// Err branch (:193-217) — verified verbatim
if let Err(audit_err) = backend.update_audit_status(...).await {
    tracing::warn!(
        app_id = %app_id, audit_id = id, transition = "Failed",
        ddl_err = %msg, audit_err = %audit_err,
        "update_audit_status failed; row stays in 'running' until reset",
    );
}
```

Field-shape unified by `7c6bd2ec` across all 5 F1 sites
(`apply.rs:163-185`, `:193-217`; `backend/postgres.rs` ×3 CIC sites)
plus the `finalise_backfill` warn at `migrations.rs:647-657` (6 sites
total). Audit row state-machine claim unchanged: the DDL error still
propagates via `result`; only secondary audit-write failure now logs
instead of swallows.

**Status: r7/r8/r9/r10 carry → CLOSED. +1 to score.**

---

## Broker waker sites — byte-identical

Re-read `broker.rs:300-369` at HEAD:

- **`Subscription::push` overflow** (`:319-321`): unchanged.
  `inner.waker.take()` + `w.wake()` inside the `borrow_mut()` borrow.
- **`Subscription::push` normal** (`:325-327`): unchanged. Same
  pattern.
- **`Subscription::close`** (`:359-369`): unchanged. r10's NEW finding
  persists verbatim.

Compio Waker semantics under verification:
`Waker::wake` enqueues to run queue without synchronous poll →
re-entry impossible today. **Latent-only.** Land as one commit when
the push fix lands.

**Status: 3 sites, MINOR-latent, carry-over from r10.**

---

## TX_CONN ownership across I6 + F1 warn-half

The forcing-function commits do not touch any `tx_conn`
take/put/drop path. The six TX_CONN mutation sites enumerated in
r9/r10 are unchanged:

- `exec.rs:51` (take in run_sql)
- `exec.rs:55` (put in run_sql)
- `transaction.rs:131-135` (take in Drop)
- `transaction.rs:239` (take in end)
- `transaction.rs:252` (clear tx_token in end)
- `orchestrator/transaction.rs:172` (defensive clear at exec_begin)

Token discipline via `debug_assert!` unchanged. F1 changes touch only
`audit.update_audit_status` (a separate pooled connection — not
tx_conn). I6 changes only the trait signature surface; no TX_CONN
interaction.

**Status: no ownership drift introduced this cycle.**

---

## MIG_LOCK ↔ audit_row coordination

The brief flags this explicitly: "the warn-half is observability-only
but the audit row state-machine claim should be unchanged."

**Verified.** Inspecting `fcf7ce3c` + `7c6bd2ec`:

- The primary error (DDL failure, data violation, INVALID index) is
  still propagated via `result` / `refuse(...)` / the outer
  `Result<T, DbError>` return.
- The audit row's `Running → Applied/Failed` transition is **still
  attempted**; only the **silently-swallowed secondary error** is now
  logged.
- No new write to `__zeroship_migrations` is introduced.
- No retry of the audit UPDATE is introduced (the F1 full sweeper
  remains deferred; this is the warn-half only).
- The audit row stays in `Running` if `update_audit_status` fails —
  same observable end-state as before, just now visible via warn log.

`MIG_LOCK` is **completely untouched** by F1. It's only manipulated
in the migrations.rs lifecycle (`set_mig_lock`, `clear_mig_lock`,
`take_mig_client`, `return_mig_client`). I6 only adds a typed Err on
the lock-release SQL; the in-process slot transitions are untouched.

`5d9acab8` (out of this cycle's forcing-function but in window)
added tracing to `set_mig_lock` (error on shadowed install) and
`return_mig_client` (warn on empty slot). Pure observability — state
machine unchanged (`replace` still replaces; empty-slot still no-ops).

**Status: audit-row + mig_lock state machines unchanged this cycle.**

---

## WAL consumer / broker fanout / PENDING_EMITS flush / cross-isolate

All untouched this cycle.

- `wal_consumer.rs`: zero diff in `d2e7e22..81226451`.
- `broker.rs`: zero diff. ConsumerRunningGuard at `:1100+` (where
  applicable) unchanged.
- `exec.rs`: `queue_or_emit`, `drain_pending_emits_on_commit`,
  `clear_pending_emits` bodies unchanged. The `ae5570dc` commit added
  +133 LOC of unit tests after `#[cfg(test)] mod tests`, no
  production-body change.
- Cross-isolate (compio single-thread per isolate) assumption
  unchanged. No new `Send`-bridging code.

---

## Findings, structured

### CARRY-OVER IMPORTANT — `run_sql` cancellation pending_emits residue (2a)

```
[IMPORTANT] crates/plugin-db/src/exec.rs:51-55 +
            crates/plugin-db/src/v8_classes/transaction.rs:240-246 +
            crates/plugin-db/src/v8_classes/transaction.rs:119-123
  Status: r6/r7/r8/r9/r10 carry, byte-identical at HEAD.
  Fix: clear_pending_emits() in BOTH early-out paths (Drop:122 and
       end:244). One-liner each.
```

### CARRY-OVER IMPORTANT — `exec_commit_batch` is_done window (2b)

```
[IMPORTANT] crates/plugin-db/src/migrations.rs:614-679
  Status: r6/r7/r8/r9/r10 carry. The I6 typed-error path widens the
       textual width of the is_done branch (was ~3 lines, now ~13
       lines) but adds ZERO new async points. Cancellation window
       semantics unchanged.
  Note: post-I6, the Err-but-not-cancelled case is observably better
       (warn instead of swallow) — but cancel-during-await race
       window is unaffected.
  Fix: MigLockGuard RAII (Drop calls clear_mig_lock). Same shape as
       ConsumerRunningGuard (70921112+386f9bf5).
```

### CARRY-OVER MINOR-latent — broker waker borrow_mut spans `w.wake()`

```
[MINOR-latent] crates/plugin-db/src/broker.rs:319-321 (push overflow)
               crates/plugin-db/src/broker.rs:325-327 (push normal)
               crates/plugin-db/src/broker.rs:359-369 (close)
  Status: r10 carry. Three sites total. Compio enqueues; not
       exploitable today. Latent if a future Waker shim polls
       synchronously and re-enters the same Subscription's RefCell.
  Fix: extract waker, drop inner, then wake. Land all three sites in
       one commit.
```

### CARRY-OVER MINOR-transient — CIC retry × pool depth

```
[MINOR-transient] crates/plugin-db/src/backend/postgres.rs (CIC loop)
  Status: r7 re-class as transient; not blocking. Unchanged this cycle.
```

### CARRY-OVER COSMETIC — OrchestratorLockGuard::release() Result is infallible

```
[COSMETIC] crates/plugin-db/src/orchestrator/lock_guard.rs:144-183
  Status: r6/r7/r8/r9/r10 carry. Unchanged.
```

### CLOSED THIS CYCLE — apply.rs `update_audit_status` Err swallow (was 2c)

```
[CLOSED-r11] crates/plugin-db/src/orchestrator/register_model/apply.rs
  Closure: fcf7ce3c (warn-half conversion at both Ok and Err branches)
  Followup: 7c6bd2ec (field-shape unification across all 5 F1 sites)
  Verification: apply.rs:163-185 + :193-217 — both branches now use
       `if let Err(audit_err) = ... { tracing::warn!(...); }` with
       unified field shape. Audit-row state machine unchanged.
```

---

## Invariants that held under r11 audit

- `386f9bf5` lazy ConsumerRunningGuard construction — unchanged.
- `e5315083` `mark_consumer_running` cfg-gating — unchanged.
- All 5 ConsumerRunningGuard lifecycle scenarios — unchanged.
- SuppressGuard ⊂ ConsumerRunningGuard lifetime nesting — unchanged.
- OrchestratorLockGuard 4-layer hardening — unchanged.
- PENDING_EMITS visibility ordering (drain after COMMIT await
  resolves AND client dropped) — unchanged.
- TX_CONN ownership — same 6 mutation sites — unchanged.
- Compio single-thread per isolate — unchanged.
- `Subscription::next` `.await` discipline — unchanged.

---

## Plateau math, refreshed

| Finding | Severity | Lift | Status |
| --- | --- | --- | --- |
| 2a run_sql cancel pending_emits | IMPORTANT | +2 | carry |
| 2b exec_commit_batch is_done window | IMPORTANT | +2 | carry |
| 2c apply.rs update_audit_status | MINOR | +1 | **CLOSED r11** |
| broker push wake ×2 + close ×1 | MINOR-latent | +0.5 | carry |
| OrchestratorLockGuard::release Result | COSMETIC | +0.5 | carry |

**r11 floor:** 88 + 1 (2c closed) = **89**.
**With 2a + 2b landed:** 89 + 4 = **93**.
**With broker waker fix (all 3 sites):** 93 + 0.5 = **93.5**.
**With cosmetic Result tighten:** 93.5 + 0.5 = **94**.

The forcing-function moved the floor by **+1**, matching the lift
r10 predicted for the apply.rs MINOR. The headline IMPORTANTs were
not touched this cycle (no `MigLockGuard` RAII, no early-out
`clear_pending_emits` in transaction.rs), so the **gating constraint
remains the two IMPORTANTs**.

---

## Score: 89 / 100 (r10: 88, Δ = +1)

**Why +1:**

- 2c (apply.rs `update_audit_status` MINOR) **closed** by fcf7ce3c +
  7c6bd2ec. Both branches warn structurally with unified field shape.

**Why not more:**

- The two IMPORTANTs (2a, 2b) are untouched. The forcing-function
  commits (I6, F1 warn-half) are **observability-only** by design —
  they fixed the diagnostic surface but not the underlying lifecycle
  hazard. r10's plateau call predicted exactly this trajectory.
- No new findings this round. The broker close site that r10
  surfaced remains carry; no fourth site found.

**Why not lower:**

- No regressions. I6 widens the is_done branch text but adds zero
  async points → cancellation window unchanged. F1 warn-half touches
  no synchronisation paths. 5d9acab8 set_mig_lock/return_mig_client
  tracing is pure observability.

| Round | Score | IMPORTANTs | New findings | Closed |
| ----- | ----- | ---------- | ------------ | ------ |
| r2  | 82   | 4 | several        | — |
| r3  | 73   | 4 | several        | — |
| r4  | 80   | 4 | 1 MINOR        | — |
| r5  | 80   | 5 | 1 MINOR        | — |
| r6  | 84   | 3 | 0              | 2 carry |
| r7  | 86   | 2 | 1 MINOR        | 1 carry |
| r8  | 88   | 2 (carry) | 0     | 1 r7-MINOR |
| r9  | 88   | 2 (carry) | 0     | 0 |
| r10 | 88   | 2 (carry) | 1 MINOR-latent (broker close) | 0 |
| r11 | **89** | **2 (carry)** | **0** | **1 (apply.rs 2c)** |

---

## Did the forcing function move the headline?

**Partially.** I6 + F1 warn-half closed the apply.rs MINOR (2c) — a
+1 lift exactly as r10's plateau math predicted. But neither
forcing-function commit is structurally capable of closing the two
IMPORTANTs:

- **2a** requires touching `Transaction::drop` / `Transaction::end`
  early-out paths to add `clear_pending_emits()`. Neither commit goes
  near `v8_classes/transaction.rs`.
- **2b** requires a `MigLockGuard` RAII type whose `Drop` calls
  `clear_mig_lock`. I6 only changes the trait signature surface;
  the lock slot lifecycle is untouched.

**The forcing function was designed for observability uplift, not
lifecycle hardening.** It did its job: F1 warn-half closes the
silent-swallow class across 6 sites (5 fcf7ce3c + finalise_backfill);
I6 makes release_advisory_lock failures observable to operators. Both
land cleanly without regressing any concurrency invariant.

**Next cycle recommendation (unchanged from r10, minus the closed 2c):**

1. Land the run_sql cancel `clear_pending_emits` two-liner. +2.
2. Land the `MigLockGuard` RAII abstraction. +2.
3. Land the broker waker fix at ALL three sites in one commit. +0.5.
4. Tighten `OrchestratorLockGuard::release()` to drop the Result. +0.5.

Each separable; total lift if all land: 89 → 94. **The headline can
move to 93 in one cycle if the two IMPORTANTs land.**

---

## Closing summary

R11's forcing function (I6 typed-error + F1 warn-half) closed the
apply.rs MINOR (+1 to floor: 88 → **89**) but did not move the
gating IMPORTANTs. Re-walked 2a byte-by-byte at HEAD — unchanged.
Re-walked 2b under the new typed-error path — the widened textual
window adds zero new async points; cancellation semantics unchanged.
Broker waker sites (`:319-321`, `:325-327`, `:359-369`) — all
byte-identical. WAL consumer, PENDING_EMITS path, TX_CONN sites,
cross-isolate surface — all untouched. The MIG_LOCK ↔ audit_row
state-machine claim holds: F1 changes are observability-only; the
audit row's Running→Applied/Failed transitions are still attempted,
just no longer silently swallowed on secondary failure.

The forcing function did its job for observability; the two
IMPORTANTs need a separate lifecycle-hardening cycle. r10's plateau
math is validated: +1 lift this cycle matched the predicted MINOR
closure exactly.

**Score: 89 / 100 (r10: 88 / 100, Δ = +1).** Headline moved by the
predicted amount; gating constraint unchanged.
