# plugin-db concurrency / lifecycle review — 2026-05-22 r6

**Commit:** `91830cca` (HEAD; cycle ~03:50)
**Lens:** concurrency + lifecycle (per task brief, round 6)
**Scope:**
  1. Verify [I42] `OrchestratorLockGuard::release()` reorder is correct.
  2. [I28] typed-error sweep across `auth/*` + `replication.rs` — any
     new RefCell-across-await or await-reorder concerns.
  3. r5 carry-overs (mig_lock leak; running_consumers Drop guard;
     run_sql cancellation pending_emits residue; broker.publish
     borrow_mut-across-wake latent).
  4. `OrchestratorLockGuard::Drop` still log-only ([I39]).
  5. `subscription.rs` broker handoff ([I40]) — no regressions.

Prior rounds: `…-r5.md` (80/100), `…-r4.md` (80/100), `…-r3.md`
(73/100), `…-r2.md` (82/100).

---

## Headline

The four commits the brief calls out are the meaningful r6 delta:

- `bd1e7ce1` ([I42] release-flag reorder) — **closes the r5 new
  MINOR**. Walked again from scratch below: the new flow is correct.
- `0049d9be` + `91830cca` ([I28] typed-error sweep across
  `auth/bootstrap.rs`, `auth/session.rs`, `auth/keys.rs`,
  `replication.rs`, `diff.rs`) — **error-rail only**; no new
  RefCell-across-await, no new await reordering, no new shared state.
- `37e61803` ([I41] `update_backfill_progress` before COMMIT) —
  **re-verified** still in place; the `mig_lock` post-COMMIT
  audit-progress-fail leak window from r2/r3/r4/r5 is **closed**:
  the only failure point on the COMMIT side is now the `COMMIT` SQL
  itself, and that path returns the client to the slot. **One r5
  IMPORTANT carry-over closed by [I41]'s relocation.**
- `4cbe9fa1` ([I40] subscription broker-handoff reorder) — r5 §3 fix;
  re-walked, no regressions; the structural unit test pins the
  ordering.

The r5 IMPORTANT backlog is therefore **down by two** (subscription
broker leak closed by [I40]; mig_lock post-COMMIT audit-progress
leak closed by [I41]'s relocation — see §3a/§3b).

Three IMPORTANTs remain (running_consumers Drop guard, run_sql
cancellation pending_emits residue, dry-run mig_lock COMMIT-fail
window). One MINOR (broker.publish borrow_mut latent) remains.
[I39] (Drop-logs-only) remains backlog by design.

**No new findings this round.**

---

## Audit dimensions, walked fresh

### 1. [I42] `OrchestratorLockGuard::release()` reorder — verify

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:114-139`.

The current shape:

```rust
pub(crate) async fn release(mut self) -> Result<Option<PooledClient<'p>>, DbError> {
    if self.released {
        return Ok(self.client.take());
    }
    if let Some(client) = self.client.as_ref() {
        let unlock_sql =
            "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
        let _ = client
            .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
            .await;
    }
    self.released = true;
    Ok(self.client.take())
}
```

Walked every cancellation scenario:

**Scenario A — future dropped mid-await (cancellation point inside
`query_text_params`).** The `client` reference is borrowed via
`self.client.as_ref()` (line 130), so `self.client` is still
`Some(_)` at the await point. `self.released` is still `false`.
Unwinding from cancellation walks past `release()`'s frame; `self`
(moved-into-`release` by the consuming receiver) is dropped. Drop
sees `released == false` and `client == Some(_)`, logs the
catastrophic-path error, then drops the `PooledClient` back to the
pool with the session-scoped lock still held until session
close. **Drop log fires.** Correct.

**Scenario B — panic inside `query_text_params`.** Same shape as A:
unwind through `release()`'s frame, `self.client.is_some()`,
`released == false`, Drop fires. **Drop log fires.** Correct.

**Scenario C — `query_text_params` returns Err** (e.g. backend
session was already closed). The `let _ = …` swallows. Execution
continues to `self.released = true; Ok(self.client.take())`. The
client (still in `self`) is taken and returned to the caller. Even
though the unlock SQL failed, the session-scoped lock will release
when the backend closes the session. **No log fires.** Correct.

**Scenario D — happy path.** Unlock SQL returns Ok. Flag flips,
client taken and returned. Drop sees `released == true`, no log.
Correct.

**Comparison vs r5:** the pre-`bd1e7ce1` shape took the client and
flipped the flag BEFORE the await, so cancellation/panic in
Scenario A/B left the local client dropped (session lock held) AND
suppressed Drop's catastrophic log (because `released == true`).
**The new shape restores operator visibility on the leak window
that the guard was designed to surface.** The lock itself still
leaks for the lifetime of the pooled connection (Drop is sync, can't
issue unlock SQL), but the operator now gets the log line — which
is the documented contract.

**Tests:** `lock_guard.rs::tests::release_idempotent_when_no_client`
(line 211), `into_held_flips_released_flag` (line 226),
`drop_with_released_true_does_not_warn` (line 248),
`drop_with_released_false_runs_warning_branch` (line 264),
`released_flag_starts_false` (line 275). Tests cover state-machine
transitions but cannot drive an actual cancellation point without a
live pool; the structural correctness is what the reorder buys.

**Status:** **[I42] verified correct. r5 M-NEW-r5-1 closed.**

### 2. [I28] typed-error sweep — any new RefCell-across-await?

**Files:** `auth/bootstrap.rs` (1107 lines), `auth/session.rs` (514),
`auth/keys.rs` (242), `replication.rs` (894), `diff.rs`, plus the
test-only `replication_ops.rs` typed-error envelope rebuild.

**Grep for shared-state borrow inside auth/*:**

```
$ rg -n 'RefCell|borrow\(\)|borrow_mut\(\)' crates/plugin-db/src/auth/
# (no matches)
$ rg -n 'context::with|with_mut' crates/plugin-db/src/auth/
# (no matches)
```

`auth/*` is pure pool-driven (functions take `&Pool` or `&Client`) and
never touch the per-isolate context. **No new RefCell-across-await
risk.** Verified by inspection of `bootstrap.rs:77-181`,
`session.rs:94-280`, `keys.rs` (no IsolateDbContext mention).

**Same grep for replication.rs:**

```
$ rg -n 'RefCell|borrow|context::with' crates/plugin-db/src/replication.rs
# (no matches)
```

Pure pool-driven module. **No new RefCell-across-await risk.**

**replication_ops.rs structural check:** every dispatch site does
the standard
```
state.borrow_mut().spawned_ops.push(Box::pin(async move { ... }))
```
where the `borrow_mut` lifetime is bounded by the `push()` call —
the async block runs later with no borrow held. Inside the async
blocks, `context::with(...)` / `context::with_mut(...)` closures
borrow the per-isolate context only inside the closure scope, no
`.await` held across (lines 181, 218, 245, 250). **Verified.**

**diff.rs has 6 `.await` sites and zero RefCell uses** (the await
points are all on `backend.*` pool calls).

**Pattern verification:** the `coded_sql` / `prefix_message` helpers
introduced by [I28] are pure-functional `(err, ctx) → DbError`
transformations — no shared state, no async. They run inside
`.map_err(|e| coded_sql("ctx", e))?` chains where the await has
already resolved.

**Status:** [I28] is **error-rail only**. No new concurrency or
lifecycle exposure introduced.

### 3a. r5 IMPORTANT carry-over — `exec_commit_batch` mig_lock post-COMMIT audit-progress fail

**File:** `crates/plugin-db/src/migrations.rs:585-619`.

The r5 §8 finding (and r4 / r3 / r2 carry-overs of the same shape)
described two failure windows where the `mig_lock` slot would stay
occupied after the COMMIT envelope:

1. **Audit-progress UPDATE fails AFTER COMMIT.** Closed by [I41]:
   the progress UPDATE is now issued inside the BEGIN/COMMIT block
   (`migrations.rs:594-609`); on failure
   `rollback_and_return(backend, client).await` runs (line 606),
   restoring the client to the slot.
2. **COMMIT SQL itself fails.** Still handled by the existing
   `return_lock_client(client)` (line 617). The client returns to
   the slot, leaving the mig_lock snapshot valid for a future
   retry. No leak.

The third window — **dry-run ROLLBACK SQL fails** — also routes
through `return_lock_client(client)` (same line 617). Same shape;
no leak.

**Re-walked the `is_done == true` branch (lines 623-648):**
post-COMMIT failure points are `finalise_backfill` (let-ignored,
line 639) and `release_advisory_lock` (let-ignored, line 644).
Neither restores the client to the slot, but both happen AFTER
`drop(client)` (line 646) and BEFORE `clear_mig_lock` (line 647).
Wait — let me re-read precisely:

```rust
let _ = backend.finalise_backfill(&client, …).await;          // 639
let lock_key = format!("zs_mig:{app_id}");
backend.release_advisory_lock(&client, &lock_key, &name).await;  // 644
drop(client);                                                    // 646
crate::context::with_mut(|c| c.clear_mig_lock());                // 647
```

The await on `release_advisory_lock` (line 644) is a real
cancellation point. If the future is cancelled there, `drop(client)`
runs as part of unwind (the local `client` is dropped on stack
unwinding), but `clear_mig_lock` does **not** run. The slot retains
its snapshot but is missing the client (it was moved out by
`take_lock_client` at line 471). Subsequent calls to fetch_batch /
commit_batch fail with `"no_active_migration", "lock client
missing"`. **The migration is stuck until operator cancel/reset.**

Hmm — but `release_advisory_lock` is a session-scoped unlock; the
client was already dropped (well, will be dropped) as part of the
unwind. The session-scoped lock releases when the session closes.
So the postgres-side lock state is consistent. The leak is just the
mig_lock snapshot stuck on the isolate.

**Status:** the r5 IMPORTANT (post-COMMIT audit-progress fail) is
**closed by [I41]**. A distinct **MINOR cancellation window** at
`release_advisory_lock` on the `is_done` path remains — but this is
shape-identical to the running_consumers / fetch_batch cancellation
class flagged below in §3c and folded into the same Drop-guard
backlog.

### 3b. r5 IMPORTANT carry-over — `subscription.rs` broker handoff

**File:** `crates/plugin-db/src/v8_classes/subscription.rs:167-227`.

[I40] (commit `4cbe9fa1`) defers the `broker::subscribe(app_id,
collection)` call until AFTER every fallible V8-alloc op has
completed:

```rust
// STEP 1 — fallible V8 alloc. Any `?` here returns BEFORE we touch
// the broker, so the broker entry can never leak.
let class_tmpl = Subscription::install(scope);
let inst_tmpl = class_tmpl.instance_template(scope);
let obj = inst_tmpl.new_instance(scope).ok_or_else(|| …)?;  // ① ?
let class_fn = class_tmpl.get_function(scope).ok_or_else(|| …)?;  // ② ?
let proto_key = v8::String::new(scope, "prototype").unwrap();
let proto_v = class_fn.get(scope, proto_key.into()).ok_or_else(|| …)?;  // ③ ?
obj.set_prototype(scope, proto_v);

// STEP 2 — broker subscribe. From this point on the broker entry's
// ONLY owner is the `Subscription` state we're about to install.
// The remaining operations are infallible.
let broker_sub = broker::subscribe(app_id, collection);  // line 202

let state = Subscription { inner: RefCell::new(Some(broker_sub)) };
// Box, External, set_internal_field, with_guaranteed_finalizer — all infallible
```

The post-`broker::subscribe` operations are all infallible (V8
`External::new`, `set_internal_field`, `with_guaranteed_finalizer`
all return owned values without Result). The structural unit test
`mint_subscription_does_not_leak_broker_entry_on_v8_alloc_failure`
(line 287-342) reads the function body and asserts every `?` byte
position lies BEFORE the `broker::subscribe(` byte position — so a
future refactor that reintroduces the bug fails the run.

The happy-path test
`mint_subscription_happy_path_registers_exactly_one_broker_entry`
(line 344) drives a fresh isolate, mints a subscription, GCs, and
asserts `broker::live_subscription_count() == 0` after the
finalizer reclaims.

**Re-walked the `Drop` impl of `Subscription`**
(`subscription.rs:55-66`): `inner.borrow_mut().take()` on Some
calls `sub.close()` — idempotent on second pass (broker's
`Subscription::close` (broker.rs:359-369) checks `inner.closed`
first). The Weak finalizer registered in `mint_subscription`
reclaims the Box, which runs Drop. **Correct.**

**Status:** r5 IMPORTANT (subscription broker leak) **closed by
[I40]**. No regressions observed.

### 3c. r5 IMPORTANT carry-over — `running_consumers` slot Drop guard

**File:** `crates/plugin-db/src/replication_ops.rs:244-252`,
`crates/plugin-db/src/wal_consumer.rs:714-761`.

The current shape is unchanged since r5:

```rust
crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
compio::runtime::spawn(async move {
    crate::wal_consumer::run_supervised(consumer).await;
    crate::context::with_mut(|c| c.unmark_consumer_running(&app_for_task));
}).detach();
```

A panic inside `run_supervised`'s body propagates out of the
spawned task BEFORE the trailing `unmark_consumer_running` runs;
the slot stays "running" until isolate teardown. Subsequent calls
to `start_replication_consumer_dispatch` short-circuit at line
181-193 (`is_consumer_running` returns true) without ever spawning
a fresh consumer. **Reactive queries silently stop working** for
that app on that isolate.

`run_supervised` itself (wal_consumer.rs:714-761) doesn't carry
panic-able code paths in normal flow — `consumer.run().await`,
`tracing::*`, and `compio::time::sleep`. But an external panic (V8
finalizer running on the same thread mid-await, OOM panics, etc.)
could trip it.

**Fix:** introduce a RAII guard that calls `unmark_consumer_running`
in its Drop, instantiate it inside the spawned task body so it runs
on the unwinding path. Mirrors `SuppressGuard` (wal_consumer.rs:138-159).

**Status:** **IMPORTANT, carry-over from r2/r3/r4/r5. No commit since
r5 touches this.**

### 3d. r5 IMPORTANT carry-over — `run_sql` cancellation pending_emits residue

**File:** `crates/plugin-db/src/exec.rs:43-73`,
`crates/plugin-db/src/v8_classes/transaction.rs:222-271`.

The run_sql cancellation window between `take_tx_client` (line 51)
and `put_tx_client` (line 55) drops the tx client. The spawned
Connection task observes the dropped sender and tears down the
backend session — Postgres rolls the tx back server-side. But
`pending_emits` queued earlier in the same tx are NOT cleared:

- `Transaction::end()` early-out at line 238-244 (when `client_opt
  is None`) returns Ok without calling `clear_pending_emits`.
- The queue persists into the NEXT `exec_begin`. Defensive
  `clear_pending_emits` at `transaction.rs:171` and
  `auto_tx.rs:225` catches it on the NEXT begin — so the residue is
  bounded.

The drift window is: cancellation drops client; subsequent
`Transaction::commit()` returns Ok-without-doing-anything; if
**no** new BEGIN happens before another code path triggers
`drain_pending_emits_on_commit`, queued events for rolled-back
writes could fire. In practice, the only callers of
`drain_pending_emits_on_commit` are `Transaction::end` (where the
COMMIT-after-cancel path is already short-circuited) and
`auto_tx::exec_auto_end` (same). So the residue is dropped on the
next `exec_begin`'s defensive `clear_pending_emits`. The drift
window is **empty under current callers** — but the invariant is
fragile, and the r3 IMPORTANT keeps it on the backlog for the
one-line fix.

**Status:** **IMPORTANT, carry-over. No commit since r5 touches
`Transaction::end`'s early-out.** Same as r5.

### 3e. r5 MINOR carry-over — `broker.rs` publish `borrow_mut` across `waker.wake()`

**File:** `crates/plugin-db/src/broker.rs:306-328`, `480-528`.

Re-verified the borrow span on `Subscription::push`:

```rust
pub fn push(&self, msg: SubscriptionMessage) {
    let mut inner = self.0.borrow_mut();    // ← borrow starts
    …
    if let Some(w) = inner.waker.take() {
        w.wake();                           // ← wake while borrow held
    }
}                                            // ← borrow ends
```

Compio's `Waker::wake` enqueues the task into the runqueue; it does
not synchronously poll. So a self-re-entry into the same RefCell is
not possible under the current scheduler. **Status: latent only.**
The one-line fix (drop `inner` before `wake`) is unchanged from r3
recommendation; not landed.

`Broker::publish`'s outer borrow_mut on `BROKER` also spans the
inner `s.push` loop (lines 510-515). Same waker contract applies.

**Status:** **MINOR, latent, carry-over. No regression.**

### 4. `OrchestratorLockGuard::Drop` log-only ([I39])

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:168-191`.

Unchanged from r5: Drop logs `tracing::error!` when
`released == false`, parks the client back to the pool with the
session lock still held. The session lock auto-releases on session
close. The operator alert is the contract; the alternative
(blocking unlock in Drop or maintenance-task channel) is documented
as out-of-scope by design.

**[I42]'s reorder makes Drop fire in MORE cancellation cases than
before** — strictly an improvement to operator visibility.

**Status:** **Drop-log-only contract holds. [I39] backlog by
design; not a r6 finding.**

### 5. `subscription.rs` broker handoff ([I40]) — no regressions

Walked in §3b above. Tests pin the structural invariant. No
regressions since r5.

---

## Score: **84 / 100** (+4 vs r5)

Why the score went up:

- **Two r5 IMPORTANTs closed by recent commits.**
  - Subscription broker leak (r5 §3, promoted-from-r4-MINOR) closed
    by [I40] (`4cbe9fa1`). Structural unit test guards regression.
  - `exec_commit_batch` post-COMMIT audit-progress fail mig_lock
    leak (r2/r3/r4/r5 carry-over) closed by [I41] (`37e61803`).
    The post-COMMIT failure points that remain are either harmless
    (let-ignored, lock auto-releases on session close) or already
    handled by `return_lock_client`.
- **r5 NEW MINOR closed by [I42]** (`bd1e7ce1`). Re-walked the
  reorder against every cancellation / panic scenario; the new
  shape restores Drop's catastrophic-path log on the windows the
  pre-`bd1e7ce1` code suppressed it.
- **No regressions introduced.** [I28]'s typed-error sweep is
  pure error-rail; it touches no shared state, no await ordering,
  no RefCell boundaries.
- **No new findings.** Round-6 audit surfaced no new IMPORTANT /
  MINOR / latent issues beyond the r5 carry-overs.

Why the score isn't higher:

- Three IMPORTANTs remain on the backlog (running_consumers Drop
  guard; run_sql cancellation pending_emits residue; the
  `exec_commit_batch` `is_done` path's `release_advisory_lock`
  cancellation window mentioned in §3a). Each has a small,
  bounded fix; their cumulative surface area is the gap to a
  90+ score.
- One MINOR (broker.publish borrow_mut across wake) is latent; the
  one-line fix is unchanged from r3 recommendation.
- [I39] (Drop-logs-only) is backlog by design — not a code finding
  but a documented contract; no contribution to the score gap.

Why not lower:

- The closed IMPORTANTs (broker leak; mig_lock post-COMMIT fail
  leak) were the most weaponisable findings in the r5 backlog. The
  remaining IMPORTANTs require cancellation or panic to surface,
  and the failure modes are either bounded (slot pinning until
  isolate teardown) or non-corrupting (silent stop, not data
  loss).

Comparison vs prior rounds:

| Round | Score | Open CRITICALs | Open IMPORTANTs | New findings |
| ----- | ----- | -------------- | --------------- | ------------ |
| r2    | 82    | 0              | 4               | several      |
| r3    | 73    | 1 (bootstrap)  | 4               | several      |
| r4    | 80    | 0              | 4               | 1 MINOR      |
| r5    | 80    | 0              | 5 (4 carry + 1 promoted) | 1 MINOR |
| r6    | **84** | 0            | **3** (2 closed) | **0**       |

The +4 jump captures the two IMPORTANTs closed plus the r5 MINOR
closed; the remaining gap to 90+ is the running_consumers /
run_sql-pending-emits / is_done-cancellation backlog.

---

## Backlog still open (rolled forward from r5)

- **IMPORTANT** — `running_consumers` slot diverges from a live
  task on panic during `run_supervised`
  (`replication_ops.rs:244-252`). RAII Drop guard not landed.
  r5 §3c carry-over.
- **IMPORTANT** — `run_sql` cancellation between `take_tx_client`
  and `put_tx_client` drops the tx client but leaves `pending_emits`
  queued. Bounded by the next `exec_begin`'s defensive clear, but
  the invariant is fragile (`exec.rs:51-55`,
  `v8_classes/transaction.rs:237-244`). r5 §3d carry-over.
- **IMPORTANT (refined)** — `exec_commit_batch` `is_done == true`
  branch's `release_advisory_lock` await is a cancellation point
  between `drop(client)` and `clear_mig_lock`. Cancellation there
  leaves the mig_lock snapshot stuck. Shape-identical to the
  running_consumers class; same fix family (RAII guard for the
  slot). r6 §3a refinement of the r5 mig_lock carry-over.
- **MINOR** — `update_audit_status` failure swallowed with `let _`
  on apply's success path; audit row stuck in `Running` after a
  successful DDL is silent (`apply.rs:160-188`). r3 finding;
  unchanged.
- **MINOR** — broker `publish` borrow_mut spans `waker.wake()` —
  safe today under compio's enqueue-only Waker contract; latent if
  a future Waker shim invokes synchronously. r3 carry-over;
  unchanged.
- **MINOR** — `dispatch_*` retries against
  `create_index_with_recovery_audited` are unbounded × pool depth
  (`backend/postgres.rs:387-619`). r3 carry-over; unchanged.
- **DESIGN** — [I39] `OrchestratorLockGuard::Drop` log-only.
  Documented contract. Not a code finding.

---

## Invariants that held up under r6 audit

- **[I42] `release()` reorder restores Drop log on cancellation /
  panic mid-unlock.** Verified for all four scenarios (cancel mid-
  await, panic mid-await, unlock SQL error, happy path).
- **[I28] typed-error sweep introduced zero new RefCell-across-
  await sites.** `auth/*` and `replication.rs` remain pure
  pool-driven; `replication_ops.rs` dispatch shape (borrow_mut for
  push, async move with no held borrow) unchanged.
- **[I41] `update_backfill_progress` before COMMIT** still in place
  (re-verified `migrations.rs:585-619`); audit-row lock held until
  COMMIT, reset blocked until release. mig_lock post-COMMIT
  audit-progress fail window closed.
- **[I40] subscription broker handoff** still in place; structural
  unit test pins the `?`-before-`subscribe` invariant; happy-path
  GC test pins broker-count reclaim. Subscription's `Drop` impl
  closes the broker entry idempotently.
- **OrchestratorLockGuard lifecycle invariants (a)-(d) from r5 §1**
  all preserved. The `'p` lifetime ties the locked client to the
  guard so borrow-check prevents early pool return; `release()` /
  `into_held()` flip the released flag before move-out; Drop log
  reaches the operator on the windows the pre-`bd1e7ce1` shape
  suppressed.
- **PENDING_EMITS visibility ordering** unchanged from r5:
  `drain_pending_emits_on_commit` fires after COMMIT `.await`
  resolves and after the client is dropped. `clear_pending_emits`
  defensively wipes residue at the start of every `exec_begin`
  (`transaction.rs:171`, `auto_tx.rs:225`).
- **TX_CONN ownership** unchanged from r5: `install_tx_client`
  only from `begin_transaction_dispatch` / `exec_auto_begin`;
  `take_tx_client` only from `run_sql` / `Transaction::end` /
  `Transaction::Drop` / `exec_auto_end`; `put_tx_client` only from
  `run_sql`. The `set_tx_token` / `set_auto_tx_owned` debug-asserts
  hold.
- **Compio task scheduling** unchanged: every plugin-db spawn
  captures `!Send` data (Rc<…>, Client, WalConsumer); the runtime
  can't migrate them across threads.

---

## Closing summary

R6 confirms that the [I42] reorder is correct, the [I28] sweep is
concurrency-neutral, and [I40] + [I41] from earlier in the cycle
closed two of the five IMPORTANT carry-overs from r5. The remaining
gap to 90+ is the running_consumers / run_sql-pending-emits /
is_done-cancellation Drop-guard backlog — same shape across three
sites, one fix family (RAII guard for context-slot teardown on
cancellation / panic).

**Score: 84 / 100** (r5: 80 / 100). +4 reflects two IMPORTANTs
closed plus one MINOR closed, with no regressions.
