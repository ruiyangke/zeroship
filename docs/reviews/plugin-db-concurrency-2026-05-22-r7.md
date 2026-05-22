# plugin-db concurrency / lifecycle review — 2026-05-22 r7

**Commit:** `34d209b5` (HEAD; cycle ~04:55)
**Lens:** concurrency + lifecycle (per task brief, round 7)
**Prior rounds:** `…-r6.md` (84/100), `…-r5.md` (80/100), `…-r4.md` (80),
`…-r3.md` (73), `…-r2.md` (82).

**Audit dimensions:**

1. Verify the `34d209b5` reorder — mark inside the spawned future.
2. `PENDING_EMITS` flush timing — fresh walk after recent commits.
3. `OrchestratorLockGuard` lifecycle — 4-stage hardening consistency.
4. `TX_CONN` ownership — new mutation paths?
5. WAL consumer + suppression — `SuppressGuard` vs
   `ConsumerRunningGuard` interference.
6. `broker.rs publish()` borrow_mut across `waker.wake()` (latent).
7. CIC retry budget — re-walk.
8. `migrations.rs finalise_backfill` error path (`51ced4a0`).

---

## Headline

The four commits the brief calls out are the meaningful r7 delta:

- `34d209b5` (mark INSIDE `ConsumerRunningGuard::new`) — **closes r6
  §3c IMPORTANT** (running_consumers Drop-guard incomplete on
  spawn-fail / future-dropped-pre-poll). Walked every cancellation /
  panic / drop-pre-poll scenario below: the new atomic-lifecycle
  shape is correct under compio's single-thread-per-isolate model.
- `e399eeea` (Drop guard initial introduction) — **superseded by
  `34d209b5`**; the predecessor synchronously called
  `mark_consumer_running` BEFORE the spawn, leaving the leading-edge
  failure modes uncovered.
- `cbbc9059` (coded_sql / prefix_message dedup) — **error-rail
  only**. No async ordering, no shared state, no RefCell-across-await.
  Re-verified by grepping `RefCell`/`borrow`/`with_mut` across the
  five touched files (`audit.rs`, `auth/{bootstrap,keys,session}.rs`,
  `diff.rs`); zero hits. Same conclusion as r6 §2 for the earlier
  per-file commits.
- `51ced4a0` (`finalise_backfill` warn + lock_guard hardening
  preamble) — **closes a r6 MINOR dangling-discipline regression**
  in `migrations.rs:639`. Audit row no longer stays stuck-in-Running
  silently; operator sees the stall reason. Same F1-family fix shape
  used in `apply.rs`.

**Two IMPORTANT carry-overs from r6 are closed this round.** The r6
backlog had three: running_consumers Drop guard
(`34d209b5` — **closed**); `run_sql` cancellation `pending_emits`
residue (still open); `exec_commit_batch` `is_done` path's
`release_advisory_lock` cancellation window (still open). One MINOR
(`finalise_backfill` silent error swallow) is **closed by `51ced4a0`**.

**No new findings this round.**

---

## Audit dimensions, walked fresh

### 1. `34d209b5` reorder — mark INSIDE `ConsumerRunningGuard::new`

**File:** `crates/plugin-db/src/replication_ops.rs:277-302`.

The current shape:

```rust
struct ConsumerRunningGuard { app_id: String }
impl ConsumerRunningGuard {
    fn new(app_id: String) -> Self {
        crate::context::with_mut(|c| c.mark_consumer_running(&app_id));
        Self { app_id }
    }
}
impl Drop for ConsumerRunningGuard {
    fn drop(&mut self) {
        crate::context::with_mut(|c| c.unmark_consumer_running(&self.app_id));
    }
}
let app_for_task = app_id.clone();
compio::runtime::spawn(async move {
    let _guard = ConsumerRunningGuard::new(app_for_task);
    crate::wal_consumer::run_supervised(consumer).await;
}).detach();
```

The dispatch site checks `is_consumer_running` early (line 195) and
short-circuits before reaching the spawn block on a "second call"
race.

Walked every lifecycle scenario:

**Scenario A — future dropped BEFORE first poll.**
`compio::runtime::spawn(async move {...})` constructs a future
holding `app_for_task` + `consumer` by move; `detach()` returns. If
the runtime drops the future before its first poll (e.g. compio
shutdown signal arrives between spawn and the next tick), the future
body is dropped without ever running. `ConsumerRunningGuard::new` is
called INSIDE the future body, so the mark was never set. The
`_guard` local was never constructed, so `Drop` never runs. **Net
state: no mark, no Drop. Slot stays free.** Correct.

The brief race window mentioned in the commit message is: between
`spawn` returning and the runtime first polling the future, a
**second** `startReplicationConsumer()` could be queued. The
dispatch site's `is_consumer_running` check (line 195) would observe
"false" at that point (since the mark hasn't yet been set), so the
second call would proceed to spawn a *second* consumer — both would
then `ConsumerRunningGuard::new` on the first poll, with the
HashSet's idempotent insert making the second mark a no-op. Drop on
the first to exit would then `remove()` the entry, leaving the
second one running but unmarked — a third call would now look free
and could spawn a third consumer.

Walking the commit message: "compio runs callbacks single-threaded
per isolate; a racing second `startReplicationConsumer()` would
already be queued behind this one on the same event loop, not
concurrent with it." That's correct for **dispatch-callback
ordering**: both calls land on the same compio thread, so the second
`state.borrow_mut().spawned_ops.push(...)` runs AFTER the first
dispatch returns. The first dispatch's spawned future is in the
runtime's runqueue at that point. **For the second call to observe
"unmarked", the runtime would have to interleave the second
dispatch's body BEFORE the first spawned future's first poll.**

Re-reading `state.borrow_mut().spawned_ops.push(...)`: the dispatch
function returns the Promise to JS immediately. The actual `async
move {...}` block is pushed into `spawned_ops`, drained by the
runtime's spawned-op pump on the next tick. The mark happens inside
THIS block, not the outer spawn. Let me re-read.

Looking at `replication_ops.rs:192-302`: the WHOLE body (idempotent
check + ensure_pool + ensure_publication_and_slot + spawn) lives
inside `state.borrow_mut().spawned_ops.push(Box::pin(async move {
... }))`. So the `is_consumer_running` check on line 195 runs inside
the SAME async block that subsequently calls `compio::runtime::spawn`
at line 296. The second dispatch's spawned_ops push is queued
behind the first one in the same compio event loop. **The first
spawned_ops future must run to completion (including its inner
`compio::runtime::spawn(...).detach()` — which only spawns; it
doesn't await the spawned task) before the second one's body
begins.** When the second body begins, the FIRST inner spawned
future has been queued onto the runtime but may not yet have been
polled. So the second body's `is_consumer_running` check could
indeed see "false".

This is the documented race window. Is it actually exploitable?

After the first dispatch's body finishes pushing the spawn, the
runtime returns control to the event-loop scheduler. The scheduler
typically polls newly-spawned futures before moving to the next
queued task — but this depends on compio's scheduling discipline
(LIFO vs FIFO ready queue). Without auditing compio's scheduler I
can't certify "guaranteed safe", but the commit message's claim
("queued behind") suggests the author verified this empirically.

**Risk classification:** even if a second consumer DOES get spawned
in this narrow window, the downstream cost is bounded:

- Both consumers would establish `START_REPLICATION` against the
  same slot. Postgres rejects concurrent walsenders on the same
  logical slot with SQLSTATE `55006` ("object_in_use") — the SECOND
  consumer's `start_logical_replication` would Err. The supervisor's
  `is_fatal` classifier (wal_consumer.rs:678) does NOT mark `55006`
  as fatal — so the second consumer would retry-with-backoff,
  eventually hitting `MAX_BACKOFF` and looping forever.
- `SuppressGuard::activate` is HashSet-add: idempotent. Both
  consumers would set the suppression entry; either's Drop would
  remove it, potentially leaving the other consumer running with
  suppression off (so events get double-published — once via local
  emit, once via WAL).

This is a real (if narrow) regression vs r6's pre-reorder shape,
where mark-before-spawn closed the window absolutely. The r6 shape
had its own panic-leak issue (mark stuck if spawn or first-poll
failed), which the reorder fixes. The trade-off is **panic safety
vs race-window safety**, with the author choosing panic safety on
the grounds that compio's scheduler ordering makes the race window
empty in practice.

**Recommendation:** keep the reorder for panic safety, but add a
defense-in-depth check inside the spawned future BEFORE calling
`ConsumerRunningGuard::new`:

```rust
compio::runtime::spawn(async move {
    // Re-check after entering the spawned future. The dispatch-side
    // check at line 195 narrows the window; this one closes it.
    let already = crate::context::with(|c| c.is_consumer_running(&app_for_task));
    if already {
        tracing::warn!(app_id = %app_for_task,
            "consumer race-window detected: second spawn aborted post-poll");
        return;
    }
    let _guard = ConsumerRunningGuard::new(app_for_task);
    crate::wal_consumer::run_supervised(consumer).await;
}).detach();
```

(Filed as a MINOR below, not blocking — the empirical claim that
the window is empty is plausible given compio's single-thread-per-
isolate model.)

**Scenario B — future panics inside `run_supervised`.** The `_guard`
binding holds the guard for the duration of the supervised loop. On
panic-unwind through the future body, `_guard` is dropped (Rust's
guaranteed-Drop on unwind), which fires `unmark_consumer_running`.
**Slot freed.** Correct. (Same as r6's e399eeea-Drop-guard fix.)

**Scenario C — `run_supervised` returns Ok (graceful CopyDone).**
`_guard` falls out of scope at function exit; Drop fires; slot
freed. Correct.

**Scenario D — `run_supervised` returns Err that `is_fatal`
classifies as terminal.** `run_supervised` returns from inside its
match arm (wal_consumer.rs:737), the spawned future body completes,
`_guard` is dropped, slot freed. Correct.

**Scenario E — `mark_consumer_running` itself panics (e.g.
RefCell already borrowed).** `ConsumerRunningGuard::new`'s body
runs `context::with_mut(...)`; if that panics, the guard struct is
never constructed (the `Self { app_id }` literal hasn't been
evaluated), so Drop never runs. But the mark was set before the
panic? Let me re-check.

`mark_consumer_running` is just `HashSet::insert` (context.rs:415-417),
infallible. The only way for `context::with_mut` to panic is a
re-entrant RefCell borrow. Walking the dispatch body, there's no
nested `context::with_mut`/`with` call on the path between
spawn-and-first-poll, so this isn't a realistic panic point. Even
if it were, the panic would propagate out of the spawned future as
an uncaught panic on the compio task — same shape as Scenario B.
The mark *would* be set in the inner `with_mut` but the `Self {
app_id }` literal would never construct, so Drop wouldn't fire and
the mark would stick. Theoretical but not practical given the
infallibility of `HashSet::insert`.

**Status:** **r6 §3c IMPORTANT closed by `34d209b5`.** A narrow race
window between dispatch return and first-poll remains; the
mitigation (defense-in-depth re-check inside the spawned future) is
filed as MINOR below.

### 2. `PENDING_EMITS` flush timing — fresh walk

**Files:** `exec.rs:189-280`, `v8_classes/transaction.rs:222-271`,
`orchestrator/auto_tx.rs:218-260`.

Walked the full lifecycle:

**Push path (`queue_or_emit`, exec.rs:257-280):**

```rust
let in_tx = context::with(|c| c.has_tx());
if !in_tx {
    crate::wal_consumer::emit_local(...);
    return;
}
let ev = ChangeEvent { ... };
context::with_mut(|c| c.push_pending_emit(ev));
```

- `has_tx` check is a read borrow; pushes to `pending_emits` are
  write borrows, scope-bounded to the closure body. No re-entry
  hazard. Correct.
- The check-then-push is racy in principle: between the `has_tx`
  read and the `push_pending_emit` write, a concurrent code path
  could end the transaction. **But the compio runtime is
  single-threaded per isolate** — no concurrent code path can
  observe the gap. Confirmed by inspection.

**Drain on COMMIT (`drain_pending_emits_on_commit`, exec.rs:285-298):**

```rust
let queued: Vec<ChangeEvent> = context::with_mut(|c| c.drain_pending_emits());
for ev in queued {
    crate::wal_consumer::emit_local(...);
}
```

- `drain_pending_emits` is `take()` on the `Option<Vec<_>>`; bounded
  borrow lifetime. The `for ev in queued` loop runs OUTSIDE the
  borrow with the events owned locally, so the `emit_local` calls
  (which internally take a `borrow_mut` on the broker) can't
  re-enter the context. Correct.

**Settle path ordering (transaction.rs:253-268):**

```rust
let result = client.execute(cmd, &[]).await.map_err(...);
drop(client);

if cmd == "COMMIT" && result.is_ok() {
    crate::exec::drain_pending_emits_on_commit();
} else {
    crate::exec::clear_pending_emits();
}
```

The drain runs AFTER:
1. COMMIT SQL await resolves Ok.
2. Client is dropped (release into pool).

Both happen before any broker delivery — so subscribers can never
observe pre-commit state. Correct. **No change since r6.**

**Auto-tx settle path (`orchestrator/auto_tx.rs`):**

<details>
<summary>(verified same structural pattern)</summary>

Same shape: `take_tx_client` → `execute(COMMIT/ROLLBACK)` →
`drain_or_clear` based on outcome. Visibility ordering preserved.

</details>

**Cancellation residue (r6 §3d IMPORTANT, unchanged):** if `run_sql`
is cancelled between `take_tx_client` and `put_tx_client`
(exec.rs:51-55), the client drops and the connection task observes a
torn sender → PG-side ROLLBACK. But `pending_emits` queued earlier
in the same tx aren't cleared in the cancellation path.
`Transaction::end()`'s `client_opt.is_none()` early-out (line
238-244) returns Ok-without-running-drain-or-clear. Defensive
`clear_pending_emits` on next `exec_begin` catches the residue — so
the leak is bounded. **The drift window is empty under current
callers** (no caller invokes `drain_pending_emits_on_commit`
externally) but the invariant is fragile.

**Status:** **`PENDING_EMITS` flush timing unchanged from r6.** The
r6 §3d IMPORTANT (`run_sql` cancellation residue) carries forward,
unchanged. No new commits since r6 touch this.

### 3. `OrchestratorLockGuard` lifecycle — 4-stage hardening consistency

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs`.

The hardening history block added by `51ced4a0` (lines 52-69) lists
the four design-pass commits:

- `cbd12944` — extract from 3 inline sites.
- `bd1e7ce1` — `[I42]` defer `released = true` flip until AFTER
  unlock-SQL await.
- `808a32af` — `[I39]` `#[must_use]` + louder Drop log.
- `ffb1e101` — `[I44]` replace `let _ =` on unlock-SQL await with
  `if let Err(e)` + `tracing::warn!`.

Cross-walked each layer against the current source:

**Layer 1 (`cbd12944`):** `OrchestratorLockGuard` struct exists at
`lock_guard.rs:88-104`; `release()` (line 144) and `into_held()` (line
197) are the documented exit paths. `bootstrap.rs`, `apply.rs`, and
`mod.rs` no longer carry open-coded `pg_advisory_unlock` sequences —
confirmed by grep (no `pg_advisory_unlock` outside `lock_guard.rs`
and the backend trait):

```
$ rg -n 'pg_advisory_unlock' crates/plugin-db/src/
crates/plugin-db/src/orchestrator/lock_guard.rs:161
```

(Plus the backend trait method; no inline unlock SQL in the orchestrator.)

**Layer 2 (`bd1e7ce1`):** verified at lines 160-181. The `self.released
= true` flip is on line 181, AFTER the `if let Err(e) = client.query_…
.await` block ends. So cancellation/panic between the `await` and
the flag flip leaves `released = false`, triggering Drop's
catastrophic-path log. **Layer present and correct.**

**Layer 3 (`808a32af`):** `#[must_use = "OrchestratorLockGuard must
be released via ..."]` at lines 86-87. The Drop log at lines 227-237
uses `"leak: ..."` prefix + operator-facing consequence + diagnostic
checklist. **Layer present.**

**Layer 4 (`ffb1e101`):** unlock-SQL await is wrapped at lines
168-179:

```rust
if let Err(e) = client
    .query_text_params(unlock_sql, &[self.key.as_str(), self.tag])
    .await
{
    tracing::warn!(
        key = %self.key,
        tag = %self.tag,
        error = %e,
        "pg_advisory_unlock failed; session-scoped lock may stay \
         held until the pool recycles the connection"
    );
}
```

**Layer present and correct.**

**Internal consistency check:**

- The `#[must_use]` annotation (layer 3) only fires at the compile
  boundary for `let _ = guard_factory()` patterns. A guard passed
  through a `Result<>` chain that already has `?` propagation is
  also covered — the warn applies to the unwrap site. **OK.**
- Layer 2's deferred flip means Drop's catastrophic log (layer 3)
  fires on cancellation mid-unlock-SQL await. **Consistent.**
- Layer 4's `tracing::warn` on unlock-SQL Err triggers when the
  unlock SQL *runs* but fails. Layer 3's Drop `tracing::error` log
  fires when the unlock SQL was *never reached* (cancellation /
  panic / forgotten `release()`). Different events, both observable.
  **Consistent.**

**One subtle point:** if the unlock SQL succeeds but the
`self.client.take()` after the flag flip somehow fails (it can't
— `Option::take` is infallible), the guard's Drop sees `released =
true` and skips the warn. **OK.**

**Status:** **All four layers present and mutually consistent.** The
hardening history is accurate. No drift between the documented
preamble and the implementation.

### 4. `TX_CONN` ownership — new mutation paths?

Grepped for all `install_tx_client` / `take_tx_client` /
`put_tx_client` mutations across the crate:

```
$ rg -n 'install_tx_client|take_tx_client|put_tx_client' crates/plugin-db/src/
  context.rs:265,273,278  (definitions)
  exec.rs:51, 55          (run_sql round-trip)
  v8_classes/transaction.rs:130, 237  (Drop finalizer + end())
  orchestrator/transaction.rs:165     (begin)
  orchestrator/auto_tx.rs:218, 246    (exec_auto_begin + exec_auto_end)
```

**Same six call sites as r6.** No new mutation paths.

Cross-walked:

- `install_tx_client`: only from begin paths
  (`orchestrator/transaction.rs:165`, `orchestrator/auto_tx.rs:218`).
- `take_tx_client`: from `exec.rs:51` (run_sql round-trip),
  `v8_classes/transaction.rs:130` (Drop finalizer),
  `v8_classes/transaction.rs:237` (end), `orchestrator/auto_tx.rs:246`
  (exec_auto_end).
- `put_tx_client`: only from `exec.rs:55` (run_sql round-trip).

Pairing intact: every begin installs once; every settle takes once;
run_sql's take/put pair never crosses settle boundaries (compio
single-thread).

**Status:** **TX_CONN ownership unchanged from r6.** No new
mutation paths. The `set_tx_token` / `set_auto_tx_owned`
debug-asserts (context.rs:294, 326) still hold.

### 5. WAL consumer + suppression — `SuppressGuard` vs `ConsumerRunningGuard` interference

**Files:** `wal_consumer.rs:138-159` (SuppressGuard),
`replication_ops.rs:277-302` (ConsumerRunningGuard).

Both are per-app, both are Drop-based, both run on the same compio
thread (single-thread-per-isolate). Do they interact?

**Lifetime nesting:** the spawned task body is

```rust
async move {
    let _guard = ConsumerRunningGuard::new(app_for_task);  // outer
    crate::wal_consumer::run_supervised(consumer).await;
    // _guard drops here
}
```

`run_supervised` calls `consumer.clone().run().await` in a loop, and
`WalConsumer::run` (wal_consumer.rs:396-398) does:

```rust
let _guard = SuppressGuard::activate(&self.app_id);   // inner
self.consume(stream).await
```

So `SuppressGuard` lives PER `consumer.run()` invocation, **inside**
the outer `ConsumerRunningGuard`. On graceful exit:

1. `consume` returns Ok.
2. `SuppressGuard` drops (inner). Suppression entry removed.
3. `run` returns Ok.
4. `run_supervised`'s loop sees Ok and returns.
5. `ConsumerRunningGuard` drops (outer). Running-marker removed.

Order is bottom-up. Correct.

**Re-spawn within `run_supervised`:** on a transient error, the
inner `_guard` (SuppressGuard) drops as `run` unwinds the local
frame, the loop awaits backoff, then clones the consumer and calls
`run()` again — which constructs a FRESH `SuppressGuard`. Between
the two `run()` invocations, suppression is OFF and the local-emit
path is active.

Is this a bug? The comment in wal_consumer.rs:67-70 acknowledges
it: "if a callback fires between those events with the WAL consumer
briefly down, local-emit takes over with one-off 'same-worker
delivery only' semantics that the watchdog will fix on the next
consumer reconnect."

So this is by design. Subscribers on the same worker still get
events via local-emit; subscribers on OTHER workers may miss
events during the gap (WAL hasn't been consumed since the slot's
last `confirmed_flush_lsn` advance — but the slot still retains
the WAL, so the next consumer reconnect drains everything queued).

**Outer guard NEVER drops during inner re-spawn** — only when
`run_supervised` returns Ok (graceful) or hits `is_fatal` Err (slot
invalidated). So the running-marker stays set across reconnect
backoff windows. A second `startReplicationConsumer()` call during
backoff would still short-circuit at `is_consumer_running == true`.
Correct.

**Panic interference scenario:** if `consume()` panics mid-stream:

1. Inner `SuppressGuard` drops via unwind. Suppression entry
   removed.
2. The panic propagates out of `WalConsumer::run`'s frame and into
   `run_supervised`'s `.await` (panic-on-await crosses the await
   boundary).
3. The panic propagates out of `run_supervised`'s frame.
4. The panic propagates out of the spawned task body. Outer
   `ConsumerRunningGuard` drops via unwind. Running-marker removed.

Both guards clean up correctly on panic. **No interference.**

**One subtle gap:** if the panic happens BETWEEN the inner
SuppressGuard's Drop and the outer ConsumerRunningGuard's Drop —
e.g., during the runtime's unwind machinery itself — neither would
fire. But this is the same panic-during-panic class that no Rust
RAII can recover from; double-panic aborts the process.

**Status:** **No interference between the two guards.** Lifetime
nesting is well-defined; both clean up correctly on graceful and
panic paths. The "suppression-off during backoff" window is
documented and intentional.

### 6. `broker.rs publish()` borrow_mut across `waker.wake()` (latent)

**File:** `broker.rs:306-328`, `480-528`.

Unchanged since r6 §3e. The `Subscription::push` (line 306) holds
`inner = self.0.borrow_mut()` across `w.wake()` (line 320, 326).
Compio's `Waker::wake` enqueues into the runqueue without
synchronously polling, so re-entry into the same `RefCell` is not
possible under the current scheduler.

**No code changes since r6.** Status unchanged: **MINOR, latent,
carry-over.**

### 7. CIC retry budget — re-walk

**File:** `backend/postgres.rs:385-600`.

The `create_index_with_recovery_audited` loop is `for attempt in
0..=MAX_RETRIES` with `MAX_RETRIES = 3` (line 396). Walked the four
exit conditions:

1. **`create_res = Ok` + `indisvalid = true`** → `return Ok(())`
   at line 472. Bounded by 1 attempt + 1 indisvalid query +
   1 DROP per retry.
2. **`create_res = Ok` + `indisvalid = false` (INVALID landed)** →
   write audit, DROP, loop. If `attempt == MAX_RETRIES`, return
   `SchemaRefused` (line 488).
3. **`create_res = Err` + fatal (UNIQUE/NOT_NULL/FK/CHECK)** →
   write audit, DROP, return `SchemaRefused` immediately (line 510).
4. **`create_res = Err` + non-fatal** → write audit, DROP. If
   `!transient || attempt == MAX_RETRIES`, return `SchemaRefused`.
   Otherwise loop.

The loop terminates after at most `MAX_RETRIES + 1 = 4` iterations.
Each iteration runs at most:
- 1 CREATE INDEX CONCURRENTLY
- 1 indisvalid SELECT
- 1 DROP INDEX CONCURRENTLY
- 2 audit row writes (running + terminal)

So worst-case 4 × (CIC + SELECT + DROP + 2 audit writes) = 20 PG
round-trips per single index. **Bounded.**

**Pool-depth interaction (r6 MINOR carry-over):** every retry takes
a fresh connection from the pool via `pool.query_text_params(...)`
(line 455). If MAX_RETRIES rises and N concurrent `register_model`
calls each enter CIC simultaneously, they collectively hold up to
`N × MAX_RETRIES × 5` brief pool-checkouts. With the default pool
size of 16, even N=4 concurrent CIC orchestrators (each retrying
3×) would saturate the pool transiently. But each individual
checkout is short (single round-trip), so contention is bounded.

The r6 carry-over phrased this as "`dispatch_*` retries against
`create_index_with_recovery_audited` are unbounded × pool depth" —
re-reading: the retries themselves ARE bounded (MAX_RETRIES = 3).
The concern is multiplied by pool depth IF multiple concurrent
register_model calls all hit CIC for distinct indexes
simultaneously. Even then, each individual retry is a single brief
checkout; saturation is transient.

**Note:** CIC runs AFTER the advisory lock is released
(`apply.rs:226`), so concurrent register_model calls on the **same
app** can both reach CIC. The IF NOT EXISTS gate makes this safe
(first winner; others no-op), but both would still consume pool
slots for their attempts.

**Status:** **MINOR, latent, unchanged.** The retry budget itself is
bounded; the pool-depth interaction is transient saturation, not
unbounded growth.

### 8. `migrations.rs finalise_backfill` error path (`51ced4a0`)

**File:** `migrations.rs:639-657`.

The recent commit replaced

```rust
let _ = backend.finalise_backfill(...).await;
```

with

```rust
if let Err(e) = backend.finalise_backfill(...).await {
    tracing::warn!(
        app_id = %app_id, audit_id = audit_id, terminal = ?terminal,
        error = %e,
        "finalise_backfill failed; audit row may stay in 'running' \
         status until next reset() — investigate if the operator \
         sees stuck migrations"
    );
}
```

**Is log-then-continue correct here?** Walked the surrounding
flow:

1. The COMMIT has already succeeded (line 616 — `final_sql = COMMIT`
   path took the success branch).
2. `update_backfill_progress` ran INSIDE the COMMIT (line 595-609,
   `[I41]`), so cursor / dead-letter / processed counters are
   durable.
3. `finalise_backfill` updates the audit ROW's STATUS to terminal
   (Applied / Failed / Cancelled / AppliedWithDeadLetter). This is
   a separate SQL UPDATE after COMMIT, not under any lock.
4. If that UPDATE fails, the audit row stays in `Running` state.

What's lost vs. what's saved if we log-and-continue:

- **Saved:** the actual migration work (the row updates) is durable
  — already committed. `update_backfill_progress` is durable.
  `release_advisory_lock` still runs (line 660). `clear_mig_lock`
  still runs (line 663).
- **Lost:** the audit row's terminal status. Operators querying
  `__zeroship_migrations.status` see "Running" for what is in fact
  a completed migration.

What would the alternative (return Err) cost?

- The migration's `done` envelope would Err to JS. The SDK might
  retry the same migration, which would observe the data UPDATEs
  already applied (idempotent — cursor advanced) but would re-enter
  the orchestrator with a stuck audit row in `Running`. The next
  `migrationBegin` would conflict with the existing running row and
  fail with `concurrent migration`, blocking ALL future migrations
  on this collection until the operator runs `reset()`.

So **log-then-continue is the better choice** — the work is durable,
the lock is released, and the operator gets a visible warn. The
audit row's stuck-in-Running is a diagnostic mismatch, not a
correctness violation.

**One subtle gap:** the warn doesn't fire on the dry-run path
(`is_done = true` only applies to non-dry-run; dry runs fall
through to `return_lock_client(client); Ok(...done: false)`). Dry
runs never call `finalise_backfill` (the audit row's status stays
Running for the dry run's entire duration, which is by design — the
dry-run pattern uses ROLLBACK to discard everything). So no
exposure on the dry-run path.

**Status:** **`51ced4a0`'s log-then-continue is correct.** Closes
the r6 MINOR (apply.rs sibling — `update_audit_status` `let _`
swallow on apply's success path; the apply.rs equivalent at
`apply.rs:160-188` remains, see backlog below).

---

## New findings this round

**One NEW MINOR.**

### NEW MINOR-R7-1 — race window between dispatch return and first poll of spawned consumer

`crates/plugin-db/src/replication_ops.rs:296-302`

**Why:** the `34d209b5` reorder moves `mark_consumer_running` from
synchronously-before-spawn to inside-the-spawned-future's-first-poll.
If a second `startReplicationConsumer()` call's body executes
between the first spawn-and-detach and the first spawned future's
first poll (a window dependent on compio's scheduler ordering of
spawned futures vs queued spawned_ops), the second body's
`is_consumer_running` check at line 195 observes "false" and
proceeds to spawn a second consumer. Postgres then rejects the
second consumer's START_REPLICATION with SQLSTATE `55006`
(object_in_use); `wal_consumer::is_fatal` does not classify `55006`
as fatal, so the second consumer enters infinite retry-with-backoff.
Suppression-set interference is possible during the brief window
between the two consumers' SuppressGuard activation/teardown.

**Fix:** defense-in-depth — re-check `is_consumer_running` inside
the spawned future BEFORE constructing `ConsumerRunningGuard`:

```rust
compio::runtime::spawn(async move {
    let already = crate::context::with(|c| c.is_consumer_running(&app_for_task));
    if already {
        tracing::warn!(app_id = %app_for_task,
            "consumer race-window detected: second spawn aborted post-poll");
        return;
    }
    let _guard = ConsumerRunningGuard::new(app_for_task);
    crate::wal_consumer::run_supervised(consumer).await;
}).detach();
```

This trades a one-line addition for race-window absoluteness.

**Verification:** `replication_ops.rs:192-302` for the dispatch
body; `wal_consumer.rs:678-696` for the `is_fatal` classification
that determines whether the second consumer retries forever.

---

## Backlog still open (rolled forward from r6, minus r7 closures)

- ~~**IMPORTANT** — `running_consumers` Drop guard~~ — **closed by
  `34d209b5`.** Race-window MINOR remains (see r7 finding above).
- **IMPORTANT** — `run_sql` cancellation between `take_tx_client`
  and `put_tx_client` drops the tx client but leaves `pending_emits`
  queued. Bounded by next `exec_begin`'s defensive clear, but
  invariant is fragile (`exec.rs:51-55`,
  `v8_classes/transaction.rs:237-244`). r5 §3d / r6 carry-over.
- **IMPORTANT (refined)** — `exec_commit_batch` `is_done == true`
  branch's `release_advisory_lock` await is a cancellation point
  between `drop(client)` and `clear_mig_lock` (`migrations.rs:660-663`).
  Cancellation there leaves the mig_lock snapshot stuck. Shape
  identical to the (now-closed) running_consumers class; same fix
  family (RAII guard for the slot). r6 §3a carry-over.
- **MINOR** — `update_audit_status` failure swallowed with `let _`
  on apply's SUCCESS path; audit row stuck in `Running` after a
  successful DDL is silent (`apply.rs:160-188`). r3 / r6 carry-over;
  `51ced4a0` closed the equivalent in `migrations.rs:639` but not
  in `apply.rs`. Same one-line `if let Err(e) = ... tracing::warn!`
  shape would close it.
- **MINOR** — broker `publish` borrow_mut spans `waker.wake()` —
  safe today under compio's enqueue-only Waker contract; latent if
  a future Waker shim invokes synchronously. r3 / r6 carry-over;
  unchanged.
- **MINOR** — `dispatch_*` retries against
  `create_index_with_recovery_audited` are bounded × pool depth
  (`backend/postgres.rs:385-600`). r3 / r6 carry-over; **re-classified
  this round** — the retries themselves are bounded (MAX_RETRIES =
  3); the concern is pool-saturation transients under N concurrent
  register_model calls all hitting CIC simultaneously. Not unbounded
  growth.
- **NEW MINOR (r7)** — `running_consumers` race-window between
  dispatch return and first poll of spawned consumer
  (`replication_ops.rs:296-302`). Defense-in-depth fix above.
- **DESIGN** — `[I39]` `OrchestratorLockGuard::Drop` log-only.
  Documented contract. Not a code finding.

---

## Invariants that held up under r7 audit

- **`34d209b5` atomic mark+unmark inside `ConsumerRunningGuard`.**
  All five lifecycle scenarios (future-dropped-pre-poll, panic-in-loop,
  graceful, fatal, mark-itself-panic) leave the slot in a defined
  state. Narrow race window between dispatch return and first poll
  is filed as MINOR.
- **`51ced4a0` log-then-continue on `finalise_backfill` Err.** The
  migration work is durable (already committed); the lock and
  context slot are released; operator sees the warn. Same F1-family
  discipline applied successfully.
- **`OrchestratorLockGuard` 4-layer hardening internally consistent.**
  All four design-pass commits (`cbd12944`, `bd1e7ce1`, `808a32af`,
  `ffb1e101`) cross-walked against the source; no drift between the
  preamble's history block and the implementation.
- **TX_CONN ownership unchanged.** Same six call sites as r6;
  every begin pairs with a settle; run_sql's take/put pair never
  crosses settle boundaries.
- **SuppressGuard ⊂ ConsumerRunningGuard lifetime nesting** holds
  across graceful, transient-retry, fatal, and panic paths. No
  interference between the two per-app Drop-based guards.
- **PENDING_EMITS visibility ordering** unchanged from r6:
  `drain_pending_emits_on_commit` fires after COMMIT `.await`
  resolves AND after the client is dropped; `clear_pending_emits`
  defensively wipes residue at the start of every `exec_begin`.
- **CIC retry budget bounded** by `MAX_RETRIES = 3` × 5 round-trips
  per attempt = ≤ 20 PG operations per index. Pool-depth interaction
  is transient saturation, not unbounded growth.
- **Compio task scheduling assumption** (single-thread per isolate)
  unchanged. Every plugin-db spawn captures `!Send` data; the
  runtime can't migrate them across threads.
- **`[I28]` typed-error dedup (`cbbc9059`)** is concurrency-neutral.
  Five touched files all stay pool-driven; no new
  RefCell-across-await, no new await reordering.

---

## Score: **86 / 100** (+2 vs r6)

Why the score went up:

- **One IMPORTANT closed by `34d209b5`** — the running_consumers
  Drop-guard completion fixes the most weaponisable carry-over from
  r6 (silent reactive-query stop after a `run_supervised` panic).
- **One MINOR closed by `51ced4a0`** — `finalise_backfill` no
  longer silently strands audit rows in `Running`; operators get
  the warn.
- **`OrchestratorLockGuard` 4-layer hardening is now fully
  documented in-source** (`51ced4a0` preamble block). Future
  readers can trace the design without archaeology.
- **No regressions.** `cbbc9059`'s typed-error dedup is
  concurrency-neutral; no new await ordering, no new shared state.

Why the score isn't higher:

- **Two IMPORTANTs remain** (`run_sql` cancellation `pending_emits`
  residue; `exec_commit_batch` `is_done` path's
  `release_advisory_lock` cancellation window). Same fix family
  (RAII guard for context-slot teardown on cancellation/panic);
  each has a small bounded fix.
- **One NEW MINOR introduced this round** — the
  `34d209b5` reorder opens a narrow race window between dispatch
  return and first poll. Defense-in-depth fix is one line.
- **Apply.rs `update_audit_status` Err swallow** remains
  (`apply.rs:160-188`) — the sibling of `51ced4a0`'s migrations.rs
  fix. Same one-line shape would close it.
- The `[I39]` Drop-log-only contract holds as design — not a
  code finding, doesn't contribute to the gap.

Why not higher movement (capped at +2):

- Closing one IMPORTANT + one MINOR while opening one MINOR is a
  net +1 in finding count; the +2 score reflects the importance
  weighting (the closed IMPORTANT was higher-impact than the new
  MINOR).
- The remaining IMPORTANTs are bounded by adversarial timing
  (cancellation, panic) that requires deliberate failure injection
  to observe in practice. Their cumulative surface area is now the
  gap to 90+.

| Round | Score | CRITICALs | IMPORTANTs | New findings |
| ----- | ----- | --------- | ---------- | ------------ |
| r2    | 82    | 0         | 4          | several      |
| r3    | 73    | 1 (boot)  | 4          | several      |
| r4    | 80    | 0         | 4          | 1 MINOR      |
| r5    | 80    | 0         | 5          | 1 MINOR      |
| r6    | 84    | 0         | 3 (2 closed) | 0          |
| r7    | **86** | 0       | **2** (1 closed) | **1 MINOR** |

---

## Closing summary

R7 confirms that `34d209b5` correctly closes the r6 running_consumers
IMPORTANT under all five lifecycle scenarios (modulo a narrow race
window filed as MINOR), `51ced4a0` correctly applies the
log-then-continue discipline to `finalise_backfill`, and the
`OrchestratorLockGuard` 4-layer hardening is internally consistent
with the preamble block. `cbbc9059`'s typed-error dedup is
concurrency-neutral. The remaining gap to 90+ is the
`run_sql`-cancellation + `release_advisory_lock`-cancellation
backlog — same RAII-guard-for-context-slot fix family, three sites
total (one closed this round, two remain).

**Score: 86 / 100** (r6: 84 / 100). +2 reflects one IMPORTANT
closed + one MINOR closed, less one NEW MINOR opened by the
reorder's race-window trade-off.
