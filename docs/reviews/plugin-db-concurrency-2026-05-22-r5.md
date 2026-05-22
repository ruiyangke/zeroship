# plugin-db concurrency / lifecycle review — 2026-05-22 r5

**Commit:** dec2bd42 (HEAD; cycle 02:50)
**Lens:** concurrency + lifecycle (per task brief, round 5)
**Scope:** OrchestratorLockGuard RAII invariants; Drop catastrophic
path consequences; subscription.rs broker-leak window; PENDING_EMITS
post-recent-commits; TX_CONN ownership; wal_consumer visibility
demotion; broker.publish borrow_mut across waker; migrations.rs
update_backfill_progress race.

Prior rounds: `…-r4.md` (80/100), `…-r3.md` (73/100), `…-r2.md`
(82/100).

---

## Headline

The five commits since r4 are net-neutral on concurrency:

- **`cbd12944` (OrchestratorLockGuard)** — pure refactor of three
  open-coded `pg_advisory_unlock` sequences into one typed guard. The
  invariant the inline sites enforced (unlock SQL before drop, on
  every error path) is preserved verbatim. The guard's `Drop` is the
  documented "catastrophic-path fallback" — a no-op on the happy and
  controlled-error paths. **One new MINOR**: cancellation / panic
  unwind during `release().await` still leaks the lock; the guard
  doesn't change that exposure (the inline code had the same shape).
- **`8ff1b2de` (auto_tx error rail)** — routes `DbError` through
  `to_op_error()` so `.code`/`.hint` survive to the SDK at the
  COMMIT/ROLLBACK boundary. No concurrency change. Verified.
- **`5ceb6daa` (visibility demotion)** — `pub fn` → `pub(crate) fn` on
  three legacy shims. **Semantically zero-impact.** Inspected the
  diff; only access modifiers change, no body / sequencing edits.
- **`dec2bd42` (migrations regression recovery)** — restores two
  unrelated fixes silently reverted by `ed697c45`. No concurrency
  change. Verified.
- **`49b0b98e` (exec subscriber gate)** — was the r4 audit subject;
  re-walked here. Still race-free under the documented single-thread
  contract.

R4's IMPORTANT carry-overs (running_consumers rapid-teardown,
exec_commit_batch mig_lock leak, run_sql cancellation pending_emits,
update_audit_status discard, broker.publish borrow_mut latent) all
remain open in r5 — no commits touched them.

**One backlog item promoted from MINOR to IMPORTANT this round:** the
r4 M-NEW-2 subscription.rs broker-leak finding. Re-walking
`mint_subscription` confirms that a failed V8 alloc after
`broker::subscribe` succeeded leaves a registered subscriber in the
broker with no path to GC. `broker::Subscription` has no `Drop` impl
that closes itself; the broker only prunes entries where
`is_closed()` is true, and `close()` is never called. This is a real
leak, not a documentation gap.

---

## Audit dimensions, walked fresh

### 1. OrchestratorLockGuard concurrency invariants

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs`,
`…/register_model/bootstrap.rs`,
`…/register_model/apply.rs`,
`…/register_model/mod.rs`.

The guard codifies four invariants the three inline sites
enforced inline pre-`cbd12944`:

**(a) Lock acquired before any DB op.**
`OrchestratorLockGuard::acquire` (lines 87-100) calls
`backend.acquire_advisory_lock(...)` and only returns Ok on success.
A failed acquire returns Err without constructing the guard, so the
caller never gets a half-acquired handle.

`bootstrap.rs:107-118` is the only call site; the guard wraps the
locked client immediately after acquire. The `build_ctx` call at
line 132 (the four post-acquire fallible operations) runs only after
the guard exists. Correct.

**(b) Lock released on every error path.**

Re-walk all three sites with the new guard:

- **bootstrap.rs:132-140**. `build_ctx` is the inner async fn;
  failure routes through the Err arm of `match`, which calls
  `let _ = guard.release().await;` and then propagates the error.
  Equivalent to the pre-refactor inline pattern. **Verified.**
- **mod.rs:199-220** (run_pipeline). `plan::compute_plan` or
  `validate::validate` failing produces `Err(...)`; the match arm at
  line 209-219 issues `lock_guard.release().await` then `return Err(e)`.
  Apply (line 225) consumes the guard if we reach it on Ok. **Verified.**
- **apply.rs:193-229**. Pass 1 is wrapped in async block to
  produce `pass1: Result<(), DbError>`. Line 226
  unconditionally calls `lock_guard.release().await` regardless of
  Pass 1 outcome, then `pass1?` propagates the error. If Pass 1
  errored, the lock is released first. If Pass 2 errors at
  line 239's `?`, the lock has already been released at line 226.
  **Verified.**

**(c) Idempotency on `release()` / `into_held()`.**

`release()` (lines 114-130): checks `self.released` first; on the
second call, takes `self.client` (already `None` after the first
call) and returns Ok(None). No double-unlock SQL. **Verified.**

`into_held()` (lines 144-156) is currently `#[allow(dead_code)]` —
no production caller. The flag-and-take pattern is identical to
`release()`'s prefix, so a future caller can't double-take.
**Verified.**

**(d) Cross-scope hand-off (bootstrap → apply).**

`bootstrap` returns `(RegisterContext, OrchestratorLockGuard<'p>)`
(`bootstrap.rs:85`). `run_pipeline` (mod.rs:171-225) keeps the guard
in scope across `plan::compute_plan` and `validate::validate`, then
moves it into `apply::apply` at line 225. The guard's lifetime
parameter `'p` ties the released-state and the held-client to the
same pool borrow, so the borrow-checker prevents anyone returning
the client to the pool ahead of the guard. **Verified.**

**Status:** All four invariants hold. The refactor preserves the
pre-`cbd12944` shape exactly. **One new MINOR follows.**

### 2. Drop catastrophic path — production consequence

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:159-182`.

The r4 code-critique M-NEW-1 noted that Drop "logs but can't await."
Walk the actual consequence chain:

**Scenario 1 — Panic during the locked region (post-acquire body).**

1. `bootstrap::bootstrap` acquires the lock; guard is constructed.
2. `build_ctx` (or `compute_plan`, `validate`, `apply` Pass 1) panics.
3. Stack unwinds past `lock_guard`. Drop runs.
4. Drop sees `self.released == false`; logs an error; falls off the
   `if !self.released` branch without doing anything else.
5. The `PooledClient` (in `self.client: Some(_)`) is dropped at the
   end of Drop. The pool's `Drop` impl on `PooledClient` returns the
   underlying connection to the pool's idle list **with the
   session-scoped advisory lock still held**.
6. Next caller pulls that connection from the pool to do
   *anything* — a CRUD operation, another register_model, an audit
   write. They get a connection whose session lock is still in PG's
   `pg_locks` table for `(zs_reg:<app>, register_model)`.
7. **Crucial**: the next caller's *connection* now holds the lock,
   but the connection's `Client` user is doing something unrelated.
   This is a **silent leak**, not a deadlock — `pg_locks` reports
   the lock owned by a session that's now servicing unrelated
   traffic. Only when *that* same connection is used for a NEW
   `pg_advisory_lock(zs_reg:<app>, register_model)` call does the
   bug surface (PG returns the lock immediately because the session
   already owns it — concurrent register_model isn't actually
   serialised any more).

So the actual production consequence is **silent loss of
serialisation** for that one `(app_id)` register_model lock across
the lifetime of the pool, not a hang. The hang only manifests if
the leaking session's PG backend dies and a different session
inherits the same `(app_id)` lock-key hash and races.

**Scenario 2 — Cancellation during `release().await`.**

This is the **new MINOR (M-NEW-r5-1) below**. `release()` consumes
self, takes the client into a local, then awaits the unlock SQL. If
the future is cancelled or panics inside `query_text_params`, the
local client is dropped and Drop on the guard is NOT called
(the guard was already moved into `release()`'s self). Same end
state as Scenario 1 — lock leaks silently.

**Scenario 3 — Normal happy path.**

`release().await` completes; `self.released = true`; `self.client.take()`
returns the now-unlocked client to the caller (or is consumed). When
the guard's Drop runs at scope exit, `self.released == true` →
warning branch skipped, no log line, no leak. **Verified.**

**Fix recommendation:** the Drop log message is the right shape for
ops alerting (a per-isolate guard tells you to look at PG `pg_locks`
for stray `zs_reg:*` entries). A complete fix requires either:
- A blocking-poll-tx unlock in Drop (compio doesn't expose this
  cleanly today; cross-thread unsafe).
- A "self-recycling" channel send from Drop to a maintenance task
  that owns a separate connection and runs the unlock.
- Acceptance that the operator-alerting Drop log is the floor and
  the silent-leak window is documented.

Today's code matches option 3 — the log is the contract. Mention in
the module docs but no code change required for r5.

### 3. subscription.rs broker leak — promoted to IMPORTANT

**File:** `crates/plugin-db/src/v8_classes/subscription.rs:157-208`,
`crates/plugin-db/src/broker.rs:156-376`, `424-443`.

R4 M-NEW-2 flagged this; re-walking confirms it's a real leak.

`mint_subscription`:

```rust
let broker_sub = broker::subscribe(app_id, collection);  // line 166
// — broker now holds a clone of broker_sub via Vec push at broker.rs:441

let class_tmpl = Subscription::install(scope);            // line 168
let inst_tmpl = class_tmpl.instance_template(scope);
let obj = inst_tmpl
    .new_instance(scope)
    .ok_or_else(|| OpError::type_error("..."))?;          // line 171-172 — ERROR PATH

let class_fn = class_tmpl.get_function(scope)
    .ok_or_else(|| OpError::type_error("..."))?;          // line 177-178 — ERROR PATH
let proto_key = v8::String::new(scope, "prototype").unwrap();
let proto_v = class_fn.get(scope, proto_key.into())
    .ok_or_else(|| OpError::type_error("..."))?;          // line 181-182 — ERROR PATH
obj.set_prototype(scope, proto_v);

let state = Subscription { inner: RefCell::new(Some(broker_sub)) };
// ... Box, External, weak finalizer ...
Ok(obj)
```

Three error paths between the `broker::subscribe` call and the
finalizer registration. On any of them, `broker_sub` (the local) is
dropped — but `broker::Subscription` has no `Drop` impl
(`broker.rs:224`). The broker's clone (pushed at `broker.rs:441`) is
unaffected, and `Subscription::is_closed()` returns false, so the
broker never GCs it on subsequent publishes.

The leaked entry then receives events into its bounded queue
(`broker.rs:514` `s.push(...)`) forever until the queue overflows
and the broker turns it into a `Resync` (`broker.rs:311-318`). The
queue then sits at 1 element forever, occupying 1 KiB-ish of
heap × N leaked subscriptions. No iterator will ever drain it.

**Severity:** IMPORTANT. The V8 alloc failures are rare in steady
state (only fire on isolate OOM), but a malicious tenant repeatedly
calling `openSubscription` against an OOM-pressured isolate could
incrementally exhaust the broker's per-app vec without any cleanup
path.

**Fix:** wrap the V8-alloc work in a closure and `broker_sub.close()`
on any Err return before propagating. The fix is local:

```rust
let broker_sub = broker::subscribe(app_id, collection);
let result = (|| -> Result<_, OpError> {
    let class_tmpl = Subscription::install(scope);
    // … allocate obj, set prototype …
    Ok(obj)
})();
let obj = match result {
    Ok(o) => o,
    Err(e) => {
        broker_sub.close();   // close BEFORE drop so broker GCs on next publish
        return Err(e);
    }
};
// proceed with Box/External/Weak setup
```

**Verification:**
- `subscription.rs:166` — broker::subscribe runs first.
- `subscription.rs:172, 178, 182` — three `?` exits before the
  finalizer is registered.
- `broker.rs:156` — `Subscription(Rc<RefCell<Inner>>)`; no `Drop` impl
  (grep for `impl Drop for Subscription` in broker.rs returned no
  matches).
- `broker.rs:492` — `subs.retain(|s| !s.is_closed())` is the only
  prune; only fires when `close()` was called on the entry.

### 4. PENDING_EMITS flush — re-audit after recent commits

**File:** `crates/plugin-db/src/exec.rs:189-298`,
`crates/plugin-db/src/v8_classes/transaction.rs:222-271`,
`crates/plugin-db/src/orchestrator/auto_tx.rs:229-275`.

The `49b0b98e` gate (subscriber-check before tuple build) doesn't
affect the COMMIT-time flush: it sits *before* `queue_or_emit`
(exec.rs:248), and `queue_or_emit` (line 257-280) is the path that
decides between immediate emit and TX-pending queueing. If the gate
returns at line 204 (suppressed or no subscribers), no event reaches
`queue_or_emit`, so `pending_emits` is not populated. If the gate
passes, events are queued and the COMMIT-time drain (`drain_pending_emits_on_commit`,
line 285-298) fires them.

**Auto-tx end path (auto_tx.rs:265-269):**
```rust
if success && result.is_ok() {
    drain_pending_emits_on_commit();
} else {
    clear_pending_emits();
}
```

Order is: take client, await COMMIT/ROLLBACK, drop client, drain or
clear. Same sequencing as the user-tx end path
(`transaction.rs:264-268`). Both run after the SQL settles, both
synchronous (no `.await` between settle and emit). **Verified.**

**The carry-over from r3/r4** — `run_sql` cancellation between
`take_tx_client` (exec.rs:51) and `put_tx_client` (line 55) drops
the client (server-side rollback) but leaves `pending_emits`
queued. The next Transaction::end's early-out (transaction.rs:238-244,
client_opt is None) returns Ok without calling
`clear_pending_emits`. The queued residue then drains on the NEXT
transaction's COMMIT, firing events for writes that were rolled
back. R3 IMPORTANT, still open, one-line fix not landed.

### 5. TX_CONN ownership — any new mutation paths?

**File:** `crates/plugin-db/src/context.rs:258-330`,
`crates/plugin-db/src/orchestrator/transaction.rs`,
`crates/plugin-db/src/orchestrator/auto_tx.rs`,
`crates/plugin-db/src/exec.rs:43-57`,
`crates/plugin-db/src/v8_classes/transaction.rs:117-271`.

No new mutation paths since r4. The path inventory is unchanged:

- `install_tx_client` (context.rs:265) — only by
  `begin_transaction_dispatch` and `exec_auto_begin`.
- `take_tx_client` (context.rs:273) — by `run_sql`,
  `Transaction::end`, `Transaction::Drop`, `exec_auto_end`.
- `put_tx_client` (context.rs:278) — only by `run_sql`.

The auto-tx `set_auto_tx_owned(true)` debug-assert (context.rs:325-328)
still holds: every `set_auto_tx_owned(true)` is gated by an active
`tx_conn`. The order in `exec_auto_begin` (auto_tx.rs:217-224) is:
install_tx_client → debug_assert previous was None → set_auto_tx_owned(true).
**Verified.**

The `set_tx_token` debug_assert (context.rs:293-298) holds: every
caller stamps the token AFTER `install_tx_client` succeeded. The
clearing path (`set_tx_token(0)`) is always permitted.
**Verified.**

**Status:** R3 IMPORTANT on `run_sql` cancellation pending-emits
residue carries verbatim; no new TX_CONN finding.

### 6. WAL consumer + suppression — visibility demotion semantics

**File:** `crates/plugin-db/src/wal_consumer.rs:130-187`.

`5ceb6daa` changed three signatures from `pub fn` to `pub(crate) fn`:
- `any_app_suppressed` (line 131)
- `set_local_emit_suppressed` (line 173)
- `local_emit_suppressed` (line 185)

`git diff 5ceb6daa^ 5ceb6daa -- crates/plugin-db/src/wal_consumer.rs`:
the only changes are the access-modifier swaps; function bodies
identical. **Semantic impact: zero.**

`local_emit_suppressed()` is called from `emit_local` at line 216 —
the call site is inside the crate, so the demotion doesn't break
the existing call. `any_app_suppressed` and
`set_local_emit_suppressed` have no production callers; the
demotion removes them from the public API surface without breaking
any tests (336 still pass per commit message).

**Status:** no concurrency / lifecycle implication. The suppression
state machine (per-app set, sentinel for the legacy thread-wide
flag, SuppressGuard's Drop-based cleanup at lines 155-159) is
unchanged.

### 7. broker.rs publish() borrow_mut across waker.wake()

**File:** `crates/plugin-db/src/broker.rs:480-528`, `604-606`.

R3 + r4 MINOR finding: the convenience accessor `publish` at line
605 holds `BROKER.borrow_mut()` for the entire `Broker::publish`
body. Inside that body, `s.push(SubscriptionMessage::Change(...))`
(line 514) ends in `inner.waker.take().wake()` at `broker.rs:319-321`
or `326-327`. The Waker is compio's, which is enqueue-only — no
synchronous re-entry into `broker::*` accessors.

The borrow-span analysis still holds in r5:

- `subs.retain(|s| !s.is_closed())` (line 492) — uses
  `s.0.borrow()` on the per-Subscription RefCell. Different RefCell
  than BROKER's. Safe.
- `for s in subs.iter() { ... s.push(...) }` (line 510-515) —
  iterates `&Vec<Subscription>` (immutable borrow of `subs`).
  Each `s.push` calls `self.0.borrow_mut()` on the per-Subscription
  RefCell. No conflict with BROKER's `borrow_mut`.
- `by_collection.remove(...)` (line 524), `self.by_key.remove(...)`
  (line 526) — happen after the `&mut subs` borrow has ended (NLL).

The argument is shape-correct today. The fragility is the same: a
future Waker that synchronously calls back into `broker::publish` /
`subscribe` / `has_subscribers` would panic with a double-borrow.
**Same MINOR as r4; no commit since touches the publish loop.**

The pre-existing r4 MINOR recommendation — snapshot `subs` into a
local `Vec<Subscription>` (each clone is an `Rc` refcount bump)
before the loop so the `BROKER.borrow_mut()` is dropped before
`s.push` fires — would close the latent re-entry concern. One-line
allocation cost per publish; small win on a hot path but not a
deal-breaker. Carried.

### 8. migrations.rs update_backfill_progress race

**File:** `crates/plugin-db/src/migrations.rs:585-608`,
`crates/plugin-db/src/audit.rs:725-754`.

The post-COMMIT progress write still runs **outside** the BEGIN /
COMMIT envelope and **without** an `audit_generation` predicate:

```rust
// migrations.rs:585-589
let final_sql = if dry_run { "ROLLBACK" } else { "COMMIT" };
if let Err(e) = backend.client_exec(&client, final_sql, &[]).await {
    return_lock_client(client);
    return Err(coded_db(&format!("migration {final_sql}"), e));
}

// migrations.rs:591-608
if !dry_run {
    if let Err(e) = backend
        .update_backfill_progress(
            &client, app_id, audit_id,
            next_cursor, dead_letter_pks, processed_total,
        )
        .await
    { ... }
}
```

`update_backfill_progress` (`audit.rs:725-754`) writes
`validate_cursor = $2, dead_letter_pks = $3, details.processed = $4`
keyed by `id` alone — no `audit_generation` clause. Operator
`migrations.reset({name, collection})` between the COMMIT (line 586)
and the progress UPDATE (line 594) bumps `audit_generation`, sets
`status='pending'`, clears the cursor — and the subsequent
`update_backfill_progress` then writes `validate_cursor = next_cursor`
**clobbering the reset**.

The Gap-X guard at `migrations.rs:506-518` does the
`audit_generation` check INSIDE the BEGIN/COMMIT envelope; it
doesn't carry past the COMMIT.

[I41] in `docs/reviews/plugin-db-deferred.md:444-451` flags this
explicitly with the smallest fix: **move
`update_backfill_progress` BEFORE the COMMIT** (2-line move).
**Verified still open** — no commit since r4 touches lines 585-608.

**Status:** IMPORTANT, unchanged from r4 and earlier rounds.

---

## New finding

### MINOR — Cancellation / panic during `OrchestratorLockGuard::release().await` leaks the lock

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:114-130`.

`release()` consumes `self`, takes the client into a local, then
awaits the unlock SQL:

```rust
pub(crate) async fn release(mut self) -> Result<Option<PooledClient<'p>>, DbError> {
    if self.released { return Ok(self.client.take()); }
    self.released = true;
    let client = match self.client.take() {
        Some(c) => c,
        None => return Ok(None),
    };
    let unlock_sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
    let _ = client
        .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
        .await;
    Ok(Some(client))
}
```

The guard's `self.released` is flipped to `true` and `self.client`
is `take()`n into a local **before** the await point. If the future
running `release()` is cancelled or panics inside
`query_text_params`, the local `client` is dropped → returned to
the pool **with the advisory lock still held on its session**. The
guard's Drop won't fire because the guard was already moved into
the consuming `release()` call.

**Why:** session-scoped advisory locks persist across the lifetime
of the pooled connection. A connection returned to the pool's idle
list with a held lock will deliver that lock to the next caller of
`pool.get()`, who will then "own" the lock invisibly until their
session ends. Subsequent `pg_advisory_lock(zs_reg:<app>,
register_model)` calls on the same session short-circuit (PG sees
the session already owns it), so concurrent register_model on that
app is no longer serialised.

**Fix:** flip `self.released = true` AFTER the await completes
successfully, not before. Or — better — keep the client inside
`self.client` (`Option<PooledClient>`) until the await returns;
then take it out, return it. Pseudocode:

```rust
let client_ref = self.client.as_ref().expect(...);
let _ = client_ref
    .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
    .await;
self.released = true;
Ok(self.client.take())
```

With this shape, cancellation between the `take_ref` and the await
leaves `self.client = Some(_)` and `self.released = false`; Drop
then fires, takes the client, and parks it back to the pool (still
with the lock held) but logs the leak. Same end state as today but
with the operator-alerting log line. Better: a separate "panicked
mid-unlock" log inside Drop so the operator can distinguish
catastrophic from normal.

**Repro:** synthetic. Wrap `apply::apply` in a future that gets
cancelled exactly when `release().await` resumes. Today the cancel
silently leaks. With the fix, Drop fires + logs.

**Verification:**
- `lock_guard.rs:118-121` — flag flip + take BEFORE await.
- `lock_guard.rs:125-128` — await on unlock SQL.
- `lock_guard.rs:159-182` — Drop only fires when
  `self.released == false`, which is no longer true after line 118.
- `compio_postgres::PooledClient`'s `Drop` returns the connection
  to the pool without issuing any unlock-on-drop SQL (the pool
  treats the connection as "warm" by default).

**Severity:** MINOR. Cancellation of the register_model future is
uncommon (no public API cancels it; isolate teardown is the main
weaponisation vector). Panic mid-unlock is similarly rare. But the
guard's documented value proposition includes catching the
catastrophic path — and the catastrophic path is still leaky in
exactly the same shape the inline code had pre-refactor. Worth
fixing as a follow-up; not a r5 blocker.

---

## Backlog still open (rolled forward from r4 / r3)

These are findings flagged in earlier rounds that no commit between
r4 and r5 addressed:

- **IMPORTANT** — `exec_commit_batch` post-COMMIT
  `update_backfill_progress` runs outside the transaction envelope
  with no `audit_generation` predicate
  (`migrations.rs:585-608`). [I41] in deferred backlog. Reset-clobber
  race window persists. r5 §8.
- **IMPORTANT** — `running_consumers` slot diverges from a live task
  on rapid teardown
  (`replication_ops.rs:249-257`). R2/R3/R4 finding; RAII Drop guard
  not landed.
- **IMPORTANT** — `exec_commit_batch` leaves `mig_lock` slot
  occupied on dry-run COMMIT failure and on the post-COMMIT
  audit-progress write failure (`migrations.rs:586-588, 605-606`).
  R2/R3/R4 finding.
- **IMPORTANT (promoted from MINOR)** — `subscription.rs::mint_subscription`
  leaks a broker entry on V8-alloc failure between
  `broker::subscribe` and the Weak finalizer registration (r4 M-NEW-2,
  r5 §3). Local fix; one closure.
- **IMPORTANT** — `run_sql` cancellation between `take_tx_client`
  and `put_tx_client` drops the tx client but leaves `pending_emits`
  queued for the NEXT tx's drain (`exec.rs:51-55`,
  `v8_classes/transaction.rs:237-244`). R3 finding; one-line fix
  in `Transaction::end` early-out arm.
- **MINOR** — `update_audit_status` failure swallowed with `let _`
  on apply's success path; an audit row stuck in `Running` after a
  successful DDL is silent (`apply.rs:160-188`). R3 finding.
- **MINOR** — broker `publish` borrow-mut spans `waker.wake()` — safe
  today under compio's enqueue-only Waker contract; latent if a
  future Waker shim invokes synchronously
  (`broker.rs:480-528`, `604-606`). R3/r4 finding.
- **MINOR (NEW r5)** — Cancellation / panic during
  `OrchestratorLockGuard::release().await` leaks the advisory lock
  silently. Reorder the `released = true` / `take` after the await.
- **MINOR** — `dispatch_*` retries against
  `create_index_with_recovery_audited` are unbounded × pool depth
  (`backend/postgres.rs:387-619`). R3 finding; pool default leaves
  margin.

---

## Invariants that held up under r5 audit

- **Advisory-lock release on every controlled error path** — the
  `OrchestratorLockGuard::release().await` call is reached on every
  Err propagation in `bootstrap` (line 137), `run_pipeline` (line
  217), and `apply` (line 226). The guard's `'p` lifetime ties the
  pool borrow to the locked client so the borrow-checker prevents
  early return of the client.
- **Idempotency of `release()` / `into_held()`** — `self.released`
  flag is checked first in `release()` and flipped before `take()` in
  both; double-call returns Ok(None) without re-issuing SQL.
- **Drop is the catastrophic-path fallback** — Drop's
  `if !self.released { tracing::error!(...) }` is reached only on
  panic unwind or missed `release()` call. The log is the operator's
  signal; the lock leak is documented.
- **Compio task scheduling for plugin-db spawns** — every spawn
  captures `!Send` data (Rc<…>, Client, WalConsumer); compio cannot
  migrate them across runtime threads. Thread-locals stay safe.
  Unchanged from r4.
- **`set_tx_token` / `set_auto_tx_owned` debug_asserts** — both
  invariants ("non-zero token implies active tx_conn",
  "set_auto_tx_owned(true) implies active tx_conn") hold under all
  current call sites. Verified in `context.rs:293-298, 324-330`.
- **`has_subscribers` gate race-freedom** — `emit_for_rows` is
  synchronous (no `.await`); broker probe and per-row build run on
  the same thread without yielding. Drain path on COMMIT re-checks
  suppression via `emit_local`. Carried from r4.
- **PENDING_EMITS visibility ordering** — drain fires after the
  COMMIT `.await` resolves and after the client is dropped. No
  subscriber observes an emit before the underlying COMMIT is
  visible to a follow-up SELECT. Carried from r4.

---

## Score: 80 / 100 (unchanged from r4)

Why the score is unchanged:

- **No regression** — none of the five new commits introduce a
  concurrency or lifecycle bug. The OrchestratorLockGuard refactor
  preserves the inline-pattern invariants; auto_tx error rail is
  pure error-rail typing; visibility demotion is access-only;
  migrations regression recovery restores prior fixes.
- **No backlog closed** — the four IMPORTANT carry-overs from r4
  (running_consumers, exec_commit_batch mig_lock leak, run_sql
  cancellation pending_emits, subscription.rs broker leak) remain
  open. The update_backfill_progress race I41 also stays open. None
  is a regression; none was fixed.
- **One MINOR promoted, one MINOR added** — the subscription.rs
  broker-leak finding (r4 MINOR) is upgraded to IMPORTANT once you
  walk the actual leak shape (`broker::Subscription` has no `Drop`,
  the broker's vec keeps the entry forever). One new MINOR on
  `release().await` cancellation. Both are small and local.

Why not higher:

- The IMPORTANT backlog from r3/r4 is genuinely real. Each of the
  five open IMPORTANTs has a one-to-three-line fix; the cumulative
  surface area is more meaningful than any single finding. The
  guard refactor is good craft (a typed pattern that future
  contributors can't accidentally violate) but it spends a commit on
  consolidation rather than draining the backlog.

Why not lower:

- The promoted IMPORTANT (subscription.rs broker leak) is bounded
  by V8 alloc failures, which is rare in steady state. The new
  MINOR (release cancellation) is bounded by future cancellation of
  the register_model path, which has no public cancel API today.
  Both are real but neither lands in normal traffic.

Comparison vs prior rounds:

| Round | Score | Open CRITICALs | Open IMPORTANTs | New findings |
| ----- | ----- | -------------- | --------------- | ------------ |
| r2    | 82    | 0              | 4               | several      |
| r3    | 73    | 1 (bootstrap)  | 4               | several      |
| r4    | 80    | 0              | 4               | 1 MINOR      |
| r5    | 80    | 0              | 5 (4 carry + 1 promoted) | 1 MINOR      |

The headline: r5's commits are quality-of-life refactors, not
backlog drain. A round 6 that closes even two of the IMPORTANTs
(the subscription.rs broker leak is a 3-line fix; the
update_backfill_progress race is a 2-line UPDATE move) would push
the score to 84-86. The two open IMPORTANTs with one-line fixes
(run_sql cancellation pending_emits, running_consumers Drop guard)
together would close the gap to mid-to-high 80s.
