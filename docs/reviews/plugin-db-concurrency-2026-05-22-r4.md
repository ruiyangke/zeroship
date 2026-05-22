# plugin-db concurrency / lifecycle review — 2026-05-22 r4

**Commit:** 40765dc8 (HEAD)
**Lens:** concurrency + lifecycle (per task brief, round 4)
**Scope:** broker two-level HashMap publish path, has_subscribers gate
race, exec_mutation_with_emit subscriber-check, WAL consumer task
lifecycle, advisory-lock release on three sites, PENDING_EMITS flush
timing, TX_CONN ownership, cross-isolate isolation, CIC retry bounds.

Prior rounds: `docs/reviews/plugin-db-concurrency-2026-05-22-r3.md`
(73 / 100), `…-r2.md` (82 / 100).

---

## Headline

The R3 CRITICAL (`bootstrap()` advisory-lock leak) is closed cleanly
by `3bb41fa1` — the async-block wrapping the post-acquire body is
exactly the pattern R3 recommended, and the error path's
`pg_advisory_unlock` SQL is issued on `lock_client` BEFORE `drop`.
That makes the register_model pipeline lock-leak-free across all
three sites (bootstrap / plan+validate / apply Pass-1).

The new commits introduce one **IMPORTANT** finding that's a fresh
shape, not a regression: the broker `publish()` loop still holds
`BROKER.borrow_mut()` across `s.push() → waker.wake()`, and the
`has_subscribers` gate added to the WAL fan-out path (`emit_for_tuple`)
and the local mutation path (`emit_for_rows`) probes the broker via
`borrow()` — both reads are sound on a single-threaded compio worker,
but documentation drift now claims more than the code proves: the
"safe because the broker and this consumer run on the same compio
thread" comment in `wal_consumer.rs:543-547` is correct, but the
`emit_for_rows` comment at `exec.rs:175-188` says "no race" without
qualifying that the gate is only race-free because callers happen
not to `.await` between the gate and the `queue_or_emit` call.

R3's IMPORTANTs that were never addressed (running_consumers
rapid-teardown, exec_commit_batch's pre/post-COMMIT mig_lock leak,
run_sql cancellation pending_emits residue, apply.rs
update_audit_status discard) all carry into r4.

---

## Audit dimensions, walked fresh

### 1. Broker borrow-mut across publish path

**File:** `crates/plugin-db/src/broker.rs:480-529`, `604-606`.

The accessor at line 604 (`broker::publish`) does

```rust
BROKER.with(|b| b.borrow_mut().publish(event));
```

so `BROKER`'s `RefCell` is mutably borrowed for the entire body of
`Broker::publish`. Inside that body:

- `self.by_key.get_mut(app)` — `&mut HashMap`.
- `by_collection.get_mut(collection)` — `&mut Vec<Subscription>`.
- `subs.retain(|s| !s.is_closed())` — `s.is_closed()` borrows
  `s.0.borrow()` on the *Subscription's* `RefCell` (a different
  `RefCell` than `BROKER`'s). Safe.
- `for s in subs.iter() { s.push(SubscriptionMessage::Change(...)) }`
  — `Subscription::push` does `self.0.borrow_mut()` on the *per-
  Subscription* `RefCell`. Different RefCell, no conflict with
  `BROKER`'s borrow.
- Inside `push`, `if let Some(w) = inner.waker.take() { w.wake(); }`
  — invokes the stored `Waker`. **This is the only point where
  control could re-enter user code synchronously**, and through
  there back into `broker::publish`/`subscribe`/`has_subscribers`,
  which would re-borrow `BROKER`.

Compio's `Waker` implementation enqueues the task on the runtime's
ready queue — no synchronous callback — so the re-entry never
happens today. The MINOR finding from R3 is unchanged: the safety
hinges on the compio runtime's waker contract, and a future Waker
shim that invoked synchronously would panic with double-borrow.

**Borrow lifetime through the by_collection / by_key remove path**:
NLL closes the `&mut subs` borrow at line 515 (the last `s` use in
the loop). The `by_collection.remove(...)` at line 524 and the
`self.by_key.remove(...)` at line 526 are then legal, because the
prior `&mut subs` (and `&mut by_collection`) borrows have ended.
Compiler-enforced.

**`has_subscribers` gate**: probes via `b.borrow().has_subscribers(...)`
(immutable). Only called from `wal_consumer::emit_for_tuple` and
`exec::emit_for_rows`, both of which are synchronous functions —
no `.await` inside. The borrow ends at the close of the `BROKER.with(...)`
closure. Safe.

**Status:** no new finding; R3's MINOR carries forward verbatim.

### 2. has_subscribers race — gate vs build

**File:** `crates/plugin-db/src/exec.rs:189-249`.

`emit_for_rows` is called from `exec_mutation_with_emit` after a
mutation's SQL has returned. The gate is:

```rust
if crate::wal_consumer::is_app_suppressed(app_id)
    || !crate::broker::has_subscribers(app_id, collection)
{
    return;
}
for row in rows { /* build tuple, queue_or_emit */ }
```

`emit_for_rows` and `queue_or_emit` are both **synchronous** (no
`.await`). The compio runtime can't yield between the gate check
and the per-row `queue_or_emit` call. So:

- A subscriber added between check and build: impossible without
  a yield point on this thread; the broker is single-threaded.
- A subscriber removed between check and build: same logic; can't
  happen. Even if a subscription's `Drop` ran, it would re-enter
  through the broker — but no `.await` exists for the drop to be
  scheduled against.
- The WAL consumer starting / stopping between check and build:
  the consumer's `suppress_app` is called from `WalConsumer::run`
  (also same compio runtime, same thread). The only window where
  `is_app_suppressed` could flip is across an `.await` — there is
  none here.

**Drain path (`drain_pending_emits_on_commit`)**: queued events are
fired via `emit_local`, which **re-checks** `is_app_suppressed`
inside its body. So if the consumer starts during a transaction
(across the COMMIT `.await`), pending emits from before the
consumer started will see the freshly-suppressed flag and skip,
correctly avoiding double-delivery. Verified at
`wal_consumer.rs:216-218`.

**Status:** the gate is race-free under the documented single-thread
contract. **MINOR**: the comment at `exec.rs:180-183` (R2's "no
race" claim) is now correct *because* the code happens to be
synchronous; a refactor that introduced an `.await` between the
gate and `queue_or_emit` would invalidate the claim without
tripping any compile-time guard.

### 3. WAL consumer task lifecycle — `running_consumers` slot

**File:** `crates/plugin-db/src/replication_ops.rs:249-257`.

R3 finding carries verbatim:

```rust
crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
compio::runtime::spawn(async move {
    crate::wal_consumer::run_supervised(consumer).await;
    crate::context::with_mut(|c| c.unmark_consumer_running(&app_for_task));
})
.detach();
```

`mark_consumer_running` runs **before** the spawn (line 250).
`unmark_consumer_running` runs **inside the spawned future**, after
`run_supervised` returns. If the spawned future is dropped before
its first poll — e.g. compio runtime shutdown during isolate
teardown — `unmark` never fires. The slot stays "running" forever
even though no task is live.

R2's recommended fix (RAII Drop guard wrapping the `app_for_task`
capture, mirroring `wal_consumer.rs::SuppressGuard` at lines
139-159) has not landed. The pattern is:

```rust
struct RunningGuard { app_id: String }
impl Drop for RunningGuard {
    fn drop(&mut self) {
        crate::context::with_mut(|c| c.unmark_consumer_running(&self.app_id));
    }
}
```

`mark_consumer_running` would happen *inside* the spawned future
together with the guard's construction, so both the mark and the
unmark are tied to the same task's lifetime. The current code's
"mark BEFORE spawn so a racing second call short-circuits" comment
(line 247) is the constraint that motivated the current ordering;
moving the mark inside the spawned future opens a tiny window where
two concurrent `startReplicationConsumer()` calls both pass the
`is_consumer_running` check. The fix is to mark before, **and** put
the unmark in a Drop guard.

**Status:** IMPORTANT, carried from R2/R3. Same recommendation, same
weaponisation profile (rapid teardown of an isolate that just
spawned a consumer).

### 4. Advisory-lock release on error — three sites walked fresh

**File:** `crates/plugin-db/src/orchestrator/register_model/bootstrap.rs`,
`apply.rs`, `mod.rs::run_pipeline`.

R3 flagged the bootstrap leak as CRITICAL. `3bb41fa1` closes it.
Walk all three sites afresh:

**bootstrap.rs:140-188** (3bb41fa1):

```rust
let inner: Result<RegisterContext, DbError> = async {
    backend.ensure_app_schema(app_id).await?;
    backend.ensure_audit_table(app_id).await?;
    let schema_version = backend.next_schema_version(app_id).await?;
    let mut declared_indexes = build_create_indexes(...)?;
    let named_indexes = build_named_indexes(...)?;
    declared_indexes.extend(named_indexes);
    Ok(RegisterContext { ... })
}.await;

match inner {
    Ok(ctx) => Ok((ctx, lock_client)),
    Err(e) => {
        let unlock_sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
        let _ = lock_client.query_text_params(unlock_sql, &[key.as_str(), LOCK_TAG]).await;
        drop(lock_client);
        Err(e)
    }
}
```

Correct: any of the four post-acquire fallible calls failing routes
through the Err arm. The unlock SQL is issued on `lock_client` (the
session holding the lock) before drop. Best-effort — unlock failure
itself is swallowed by `let _`, which is the right call (if the
session is dead, the lock auto-released; if not, the connection
returning to the pool with a held lock is the failure we *can't*
detect from here).

**Lock-key invariant pin** (`bootstrap.rs:218-228`): `lock_key()`
returns `format!("zs_reg:{app_id}")` and `LOCK_TAG` is the literal
`"register_model"`. The unit test asserts the format string; if a
future patch drifts either of these, the unlock SQL stops releasing
what `acquire_advisory_lock` took. Good guardrail.

**run_pipeline mod.rs:211-228** (b4e533e2): on plan/validate Err,
issues `pg_advisory_unlock` then `drop(lock_client)`. Same pattern
as bootstrap. Correct.

**apply.rs:201-233** (37a0ef76): Pass 1 in async block, capture
`pass1: Result`. Then **unconditionally** issues `pg_advisory_unlock`
(line 224-229) regardless of Pass 1 outcome. Then `pass1?`
propagates the error. Pass 2 runs unlocked. Correct.

**One latent concern**: the `let _ = lock_client.query_text_params(...).await;`
pattern swallows the unlock-SQL error. If the unlock SQL itself
returns Err (e.g. session lost — `lost connection` error), the lock
state from the server's perspective is "session died, lock auto-
released", which is fine. If the unlock SQL returns Err for any
*other* reason (PG bug, malformed call, transient), the lock stays
held and the connection still returns to the pool — same shape as
the original bug. Not a real concern in practice (no realistic
non-disconnect path returns Err from `pg_advisory_unlock`), but
the swallowing is mentioned for completeness.

**Status:** all three sites correct. R3's CRITICAL closed.

### 5. PENDING_EMITS flush timing — subscriber visibility vs COMMIT

**File:** `crates/plugin-db/src/v8_classes/transaction.rs:222-269`,
`crates/plugin-db/src/orchestrator/auto_tx.rs:203-249`,
`crates/plugin-db/src/exec.rs:285-298`.

The COMMIT path in `transaction::end`:

1. `client.execute("COMMIT", &[]).await` — COMMIT is durable when
   this resolves.
2. `drop(client)` — connection returns to pool / closes.
3. `drain_pending_emits_on_commit()` — fires queued events
   synchronously through `emit_local` → `publish`.

A subscriber's `find()` reads through the pool/tx_conn. After step
1 resolves, any new pool query sees the committed rows. Step 2 is
synchronous (Rust Drop). Step 3 is synchronous (no `.await`). So
a subscriber that receives the event via `publish` at step 3 and
then issues a `find` will see the committed data — visibility holds.

But: the broker fires events synchronously via `s.push() →
waker.wake()`. The subscriber's `next()` future is woken — but the
wake is enqueue-only on compio. The subscriber task hasn't yet
polled. By the time the subscriber's poll runs, even more wall-time
has passed. The COMMIT is even more durable. So the invariant
"subscriber observes event ⇒ COMMIT visible to SELECT" holds.

**Cross-process visibility**: the WAL consumer path (the cross-
worker dual write) publishes through the same `publish()` function
on the consumer's thread. Postgres's logical replication doesn't
forward an INSERT until COMMIT is in WAL, so the WAL consumer
observes the COMMIT *after* it lands. Visibility holds across
workers too.

**Status:** correct. No new finding. The R2 review hand-waved this
as "Gap B closure"; r4 confirms the sequencing.

### 6. TX_CONN ownership lifecycle

**File:** `crates/plugin-db/src/context.rs:89-330`,
`crates/plugin-db/src/v8_classes/transaction.rs:117-269`,
`crates/plugin-db/src/orchestrator/transaction.rs:38-200`,
`crates/plugin-db/src/orchestrator/auto_tx.rs:147-250`.

Mint at `begin_transaction_dispatch` (`orchestrator/transaction.rs:81`):
`install_tx_client(client)`, `set_tx_token(token)`. Both inside one
`with_mut` block — atomic on this thread.

Hold across awaits: `run_sql` (`exec.rs:51-55`) does `take_tx_client`
→ `await query` → `put_tx_client`. Between the take and the put,
the slot is `None` but `tx_token != 0`. R3's IMPORTANT noted: if the
spawned op future is dropped between take and put, the client is
dropped (server-side rollback), `tx_token` stays non-zero,
`pending_emits` stays queued. `Transaction::end`'s early-out branch
at `v8_classes/transaction.rs:237-244` does NOT call
`clear_pending_emits` (only the Drop path at line 137 does). Same
finding, unchanged in r4.

Release at end: `Transaction::end` takes the client, executes
COMMIT/ROLLBACK, drops the client, drains/clears pending emits.
`set_tx_token(0)` happens before the await so the finalizer/GC sees
"settled" if it races. Correct.

Drop finalizer (`v8_classes/transaction.rs:117-138`): checks token
match before acting, takes client, drops it (server-side rollback),
clears pending emits. Correct.

Auto-tx (`orchestrator/auto_tx.rs:203-249`): symmetric to user-tx,
with the simpler invariant that `auto_tx_owned = true` ⇒ `tx_conn =
Some` (debug_assert at `context.rs:325-328`).

**Status:** R3's IMPORTANT on `run_sql` cancellation pending-emits
residue carries verbatim. No new TX_CONN finding.

### 7. Cross-isolate isolation

`compio::runtime::spawn` requires `'static` but does NOT require
`Send` — verified by the fact that the spawned tasks below all
capture `Rc<…>` (which is `!Send`):

- `replication_ops.rs:251` — captures `consumer: WalConsumer` and
  `app_for_task: String`.
- `orchestrator/auto_tx.rs:179` — connection task captures
  `connection`.
- `orchestrator/transaction.rs:152` — connection task.
- `v8_classes/migration.rs:120` — captures `backend: Rc<PostgresBackend>`,
  `owner: MigrationOwner` (both `!Send`).
- `lib.rs:266` — pool connection task.
- `backend/postgres.rs:76` — pool init.

Every one is `!Send`. compio's scheduler can't migrate `!Send`
tasks across runtime threads, so all of these stay on the worker's
single compio runtime. Thread-locals (`BROKER`, `ISOLATE_CTX`,
`SUPPRESSED_APPS`, `MODEL_REGISTRY`, `READ_SET_BUILDER`) are
accessed only from within these tasks.

**Status:** invariant holds. R3 confirmed; nothing new to add.

### 8. CIC retry loop bounded under concurrent register_model

**File:** `crates/plugin-db/src/backend/postgres.rs:387-619`.

`MAX_RETRIES = 3` at line 398. Loop bound at line 456:
`for attempt in 0..=MAX_RETRIES` — 4 iterations max per CIC call.

Per iteration: `pool.query_text_params(create_sql, [])` (1 borrow),
validity check (1 borrow), optional drop_idx_sql (0–1 borrow), audit
writes (1–3 borrows for log_retry / log_terminal / log_audited).
Sequential — each `.await` completes before the next.

With N concurrent register_model on N apps, all CIC writes share
the same pool. Pool default size is configured by
`compio-postgres`'s `Pool::DEFAULT_MAX_SIZE` (16 last I checked).
The retry loop's audit fan-out can spike to ~6 borrows per attempt
× N apps under a multi-tenant deploy storm. Sequential within an
app means at most one borrow held at a time per app's CIC, so the
worst-case in-flight count is N (one per concurrent CIC). Below
pool depth, no exhaustion. Above pool depth, some `Pool::get()`
calls block waiting — but the wait is bounded by the running ops.

The R3 MINOR on "retry fan-out unbounded × pool size" remains
true in shape but is not a real exhaustion risk under default
configuration. Worth noting; not actionable today.

**Status:** R3 MINOR carries. No change.

---

## New finding

### MINOR — `subs.retain` removes closed entries inside a `borrow_mut`-held loop; doc claims more isolation than the code provides

**File:** `crates/plugin-db/src/broker.rs:491-516`.

The `publish()` method does:

```rust
subs.retain(|s| !s.is_closed());        // line 492 — calls s.0.borrow()
let is_empty = subs.is_empty();
if !is_empty {
    let shared = Rc::new(event.clone());
    for s in subs.iter() {
        if !s.accepts(&shared) { continue; }    // calls s.0.borrow()
        s.push(SubscriptionMessage::Change(Rc::clone(&shared)));  // calls s.0.borrow_mut() + waker.wake()
    }
}
```

The `BROKER.borrow_mut()` from the outer `BROKER.with(|b| b.borrow_mut().publish(event))`
accessor (line 605) is held throughout. Inside, each `s.is_closed()`,
`s.accepts(&shared)`, and `s.push(...)` borrow the **Subscription's**
inner RefCell — a different RefCell than BROKER's.

The risk: if `waker.wake()` synchronously invoked user code (it
doesn't on compio, but the future Waker contract permits it), and
that user code called any `broker::*` accessor that re-acquired
`BROKER.borrow()` or `BROKER.borrow_mut()`, it would panic. The
comment at `broker.rs:508-509` claims this is safe because "no
recursive publish" — but the actual safety argument is "the compio
runtime's Waker is enqueue-only".

A more accurate comment would name the actual invariant. Or, the
fix R3 already proposed: snapshot `subs.clone()` into a local Vec
before the loop, drop the `&mut subs` borrow (and therefore the
`BROKER.borrow_mut()`'s last meaningful tie-in to `by_key`), then
iterate the snapshot calling `s.push`. Each `s.push` would still
acquire its own `Subscription` RefCell — but the broker's
`borrow_mut` could be dropped before the loop. Concrete cost: one
`Vec<Subscription>` alloc per publish (each Subscription is
`Rc<RefCell<…>>` so the clone is a refcount bump). Not free, but
the safety story becomes self-contained.

**Why:** the safety argument has subtly shifted from "broker is
single-threaded so no concurrent borrow" (true, but doesn't address
re-entry) to "compio's Waker is enqueue-only" (true today, fragile
under future runtime evolution). The new MINOR is about the
documentation-vs-mechanism gap, not a real panic today.

**Fix:** either snapshot before the loop (eliminates the
`borrow_mut`-across-waker-fire concern entirely), or update the
comment to name the actual invariant (compio Waker contract).

**Verification:**
- `broker.rs:480-516` (publish loop).
- `broker.rs:319-327` and `325-327` (push body fires waker under
  Subscription's inner `borrow_mut`).
- `broker.rs:604-606` (accessor holds `BROKER.borrow_mut()` for
  the publish duration).
- `v8_classes/subscription.rs:106-120` — the only place waker
  is registered; uses `cx.waker().clone()` from compio's poll
  context.

---

## R3 backlog still open

These were findings in earlier rounds that haven't been addressed
between r3 and r4:

- **IMPORTANT** — `exec_commit_batch` returns the lock client after a
  successful COMMIT when the audit-progress write fails
  (`migrations.rs:553-577`). The mig_lock slot stays `Some` with the
  client still parked + advisory lock still held on its session.
  R3 finding. No commit between r3 and r4 touches this path.
- **IMPORTANT** — `running_consumers` slot diverges from a live task
  on rapid teardown (`replication_ops.rs:249-257`). R2/R3 finding;
  fix (RAII Drop guard) not landed.
- **IMPORTANT** — `exec_commit_batch` leaves `mig_lock` slot occupied
  on dry-run COMMIT failure (`migrations.rs:553-558`). R2/R3 finding.
- **IMPORTANT** — `run_sql` cancellation drops the tx client but
  doesn't clear `pending_emits` if the cancellation happens between
  `take_tx_client` and `put_tx_client` (`exec.rs:43-57` +
  `v8_classes/transaction.rs:237-244`). R3 finding; one-line fix
  not landed.
- **MINOR** — `update_audit_status` failure leaves the audit row
  stuck in `Running` after a successful DDL
  (`apply.rs:160-188`). R3 finding; the `let _ = …` swallow is
  unchanged.
- **MINOR** — broker `publish` is not re-entry safe if a Waker
  callback synchronously re-publishes (`broker.rs:480-528`,
  `604-606`). R3 finding; the two-level HashMap refactor doesn't
  change the borrow span. See the new MINOR above for a refined
  characterisation.
- **MINOR** — `dispatch_*` retries against
  `create_index_with_recovery_audited` are unbounded × pool depth
  (`backend/postgres.rs:387-619`). R3 finding; pool default leaves
  margin.

---

## Invariants that held up under r4 audit

- **Advisory-lock release on every error path** — all three sites
  (bootstrap, plan/validate, apply Pass-1) now release the lock
  via `pg_advisory_unlock` SQL before dropping `lock_client`. The
  contract between `acquire_advisory_lock` SQL and `pg_advisory_unlock`
  SQL is pinned by a unit test on `lock_key()` / `LOCK_TAG`.
- **Broker two-level HashMap** — borrow lifetime through
  `by_collection.get_mut` → `subs.iter()` → `by_collection.remove`
  is sound under NLL. Per-app inner HashMap is correctly dropped
  when the last collection empties (test at `broker.rs:1299-1310`).
- **has_subscribers gate is race-free** — `emit_for_rows` and
  `emit_for_tuple` are synchronous; no `.await` between gate and
  build. Drain path on COMMIT re-checks suppression via
  `emit_local`'s internal `is_app_suppressed` guard.
- **DropColumn/DropIndex invariant gate** — `apply.rs::run_op`
  explicitly errors with `DbError::Internal` when a destructive
  op survives the upstream class filter (commit `3ef6a170`). The
  previous silent `Ok(())` would have written a fake `Applied`
  audit row.
- **Empty-RETURNING in replication** — `c83d6a8c` correctly turns
  the empty-RETURNING from `pg_create_logical_replication_slot`
  into a `DbError::Internal` instead of a silent `lsn = ""` sentinel.
  Same shape as `d7cfc089`'s audit-id=0 fix. Both have unit tests.
- **PENDING_EMITS COMMIT-visibility** — drain happens after the
  COMMIT `.await` resolves and after the client is dropped. No
  subscriber can observe an emit before the underlying COMMIT
  is visible to a follow-up SELECT.
- **Compio task scheduling** — every plugin-db spawn captures
  `!Send` data (`Rc<…>`, `Client`, `WalConsumer`), so the compio
  runtime cannot migrate them across threads. Thread-locals are
  safe.

---

## Score: 80 / 100  (up from 73)

Why the jump:

- **CRITICAL closed** — the bootstrap advisory-lock leak that drove
  r3's score down by 9 points is fully fixed by `3bb41fa1`. The fix
  is the exact pattern r3 recommended (async-block wrap, unlock SQL
  before drop). The unit-test pin on `lock_key`/`LOCK_TAG` is a
  small but real guardrail against silent drift.
- **Two recent commits exercise the right concurrency invariants**:
  `49b0b98e` adds the subscriber-check gate to the local emit path,
  mirroring the WAL fan-out fix from `967a7362`; `0e58c4e8` +
  `b32ba383` make the gate alloc-free via the two-level HashMap
  layout. Both changes are race-free under the documented single-
  thread invariant, and both pin behaviour with new unit tests.
- **One new MINOR** about the broker publish-loop documentation
  drift vs the actual safety mechanism — small, not blocking.

Why not higher:

- The R3 backlog hasn't been worked. Four IMPORTANTs that R2 or R3
  flagged are still open. None is a regression, but the cumulative
  surface area is real:
  - migrations.rs has TWO COMMIT-fail / audit-fail paths that leak
    `mig_lock` + advisory lock on the parked client. Either of those
    landing wedges the next migration on the same isolate.
  - `running_consumers` rapid-teardown wedges subsequent
    `startReplicationConsumer` calls on the same app/isolate after
    an evict-during-spawn.
  - `run_sql` cancellation residue in `pending_emits` is harder to
    weaponise but the fix is one line and the symmetric Drop path
    already does the right thing.

- The broker-publish borrow-while-firing-waker concern is real on
  paper. Today it's safe because compio doesn't expose the Waker
  contract's synchronous option; tomorrow's runtime (or a Waker
  passed across an FFI shim) could regress it. The fix is local
  (snapshot the subs Vec before the loop) and cheap.

Comparison vs r3 (73 / 100):

| Round | Score | Open CRITICALs | Open IMPORTANTs | Open MINORs |
| ----- | ----- | -------------- | --------------- | ----------- |
| r2    | 82    | 0              | 4               | several     |
| r3    | 73    | 1 (bootstrap)  | 4               | several     |
| r4    | 80    | 0              | 4 (same as r3)  | several+1   |

The headline: r3's CRITICAL is gone, three recent commits exercise
concurrency invariants correctly, but the R3 IMPORTANT backlog
remains untouched between r3 and r4. The advisory-lock story is now
clean; the mig_lock / running_consumers / pending_emits stories are
where the next round of work needs to focus. A round 5 that closes
even half of the open IMPORTANTs would push the score into the
mid-to-high 80s.
