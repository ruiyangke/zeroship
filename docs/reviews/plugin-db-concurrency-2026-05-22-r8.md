# plugin-db concurrency / lifecycle review — 2026-05-22 r8

**Commit:** `f6043126` (HEAD; cycle 08:00, post-r7)
**Lens:** concurrency + lifecycle (round 8, fresh re-audit)
**Prior rounds:** `…-r7.md` (86/100), `…-r6.md` (84), `…-r5.md` (80),
`…-r4.md` (80), `…-r3.md` (73), `…-r2.md` (82).

**Commits since r7:**

- `386f9bf5` — ConsumerRunningGuard lifted to module scope + 4 lifecycle
  tests + LATENT BUG FIX (`then_some` → `then(||)`).
- `e5315083` — `mark_consumer_running` gated behind `cfg(any(test,
  feature = "test-helpers"))`; production-only API is now
  `try_mark_consumer_running`.
- `deeefe18` — `migrations::coded_db` routes through shared
  `prefix_message` (no concurrency change).
- `f6043126` — SQLSTATE-typed checks replace substring matches in
  `replication.rs:213,257` (no concurrency change).

**Audit dimensions (per brief):**

1. Verify `386f9bf5` latent bug fix — `won.then(|| Self {…})` lazy
   evaluation on the lost-race path.
2. `ConsumerRunningGuard` atomic semantics — `try_mark_consumer_running`
   uses `HashSet::insert` (returns true iff newly inserted).
3. Drop on panic — verified by `guard_drop_unmarks_on_panic_unwind`.
4. `PENDING_EMITS` flush timing — fresh walk.
5. `OrchestratorLockGuard::release()` cosmetic Result.
6. WAL consumer task lifecycle — `running_consumers` slot + Drop.
7. `broker.rs` `publish` borrow_mut across `waker.wake()` (latent).
8. `TX_CONN` ownership — fresh re-walk.

---

## Headline

The two commits with concurrency surface area in this cycle —
`386f9bf5` (latent bug fix) and `e5315083` (production API gating) —
are **both net improvements**. The latent bug fix is the most
significant correctness change since r7's `34d209b5` reorder: the
`then_some(Self { app_id })` formulation would have **silently broken
70921112's atomic try-claim** on the lost-race path, leaving BOTH
racing tasks without a consumer mark. `386f9bf5`'s lifecycle tests
catch this at unit-test time; the `then(||)` lazy formulation is now
correct.

`e5315083`'s gating of `mark_consumer_running` behind `cfg(test/
test-helpers)` **closes the api-surface footgun**: a contributor
picking the non-atomic variant over the atomic one in production
would have re-opened the race window. The atomic
`try_mark_consumer_running` is now the only non-test surface.

**One r7 finding is closed this round:**

- NEW MINOR-R7-1 (race window between dispatch return and first poll
  of spawned consumer) — superseded by the atomic try-claim
  (`70921112` + `386f9bf5`). The window the r7 finding described is
  now closed BY CONSTRUCTION inside the spawned future. **Closed.**

**No new findings this round.** Two r6 IMPORTANTs remain (run_sql
cancellation `pending_emits` residue; `exec_commit_batch` is_done
`release_advisory_lock` cancellation window). One apply.rs MINOR
remains (`update_audit_status` `let _` swallow).

---

## Audit dimensions, walked fresh

### 1. `386f9bf5` latent bug fix — `won.then(|| Self { app_id })` lazy

**File:** `crates/plugin-db/src/replication_ops.rs:346-360`.

Current shape:

```rust
impl ConsumerRunningGuard {
    fn try_claim(app_id: String) -> Option<Self> {
        let won = crate::context::with_mut(|c| c.try_mark_consumer_running(&app_id));
        won.then(|| Self { app_id })
    }
}
```

**Walk of the lost-race path (`won == false`):**

1. `try_claim(app_id: String)` enters with `app_id` owned.
2. `with_mut(|c| c.try_mark_consumer_running(&app_id))` borrows `app_id`
   for the closure body; `HashSet::insert` returns `false` because
   another task already inserted. Borrow ends; `app_id` is still owned.
3. `won.then(|| Self { app_id })` — `bool::then` is the lazy variant
   accepting `FnOnce() -> T`. The closure captures `app_id` by move
   (it must, to evaluate `Self { app_id }`).
4. Since `won == false`, the closure is **NOT invoked**. The closure
   value is dropped at the end of the statement. Dropping the closure
   drops its captured `app_id` (a `String`) — which has no custom
   Drop impl beyond freeing the heap buffer.
5. `Self` is **never constructed**. `ConsumerRunningGuard::drop` is
   **never invoked**. The winner's mark is preserved.

**Walk of the won-race path (`won == true`):**

1. Same as steps 1-2.
2. `HashSet::insert` returns `true` (newly inserted).
3. `won.then(|| Self { app_id })` invokes the closure. `Self { app_id
   }` is constructed and the closure returns it. `Some(Self {…})`.
4. The mark persists until this `Self` is later dropped by the
   spawned task's exit (graceful, panic, or future-dropped-pre-poll).

**Compare to the buggy `then_some(Self { app_id })`:** `bool::then_some`
is the EAGER variant `then_some(t: T) -> Option<T>`. The `Self { app_id
}` literal would be evaluated **before** `then_some` runs the won-check.
On `won == false`, the literal would be constructed (consuming
`app_id`), then `then_some` would return `None`, dropping the ephemeral
`Self`. The `Drop` impl would fire `unmark_consumer_running(&self.
app_id)` — clearing the WINNER's mark. Net result: both racing tasks
end up with no consumer running.

**The new test pins this exactly** — `replication_ops.rs:414-424`:

```rust
#[test]
fn consumer_running_guard_try_claim_loses_when_already_marked() {
    reset_consumer_registry("guard_t4");
    let g1 = ConsumerRunningGuard::try_claim("guard_t4".into()).expect("first claim wins");
    let g2 = ConsumerRunningGuard::try_claim("guard_t4".into());
    assert!(g2.is_none(), "second concurrent claim must return None");
    // First guard still holds the mark.   <-- THIS line traps the bug
    assert!(context::with(|c| c.is_consumer_running("guard_t4")));
    drop(g1);
    assert!(!context::with(|c| c.is_consumer_running("guard_t4")));
}
```

A silent revert to `then_some(...)` would fire the Drop on line 418's
return, clearing the mark before line 421 checks it — assertion fails.

**Status:** **`386f9bf5`'s latent bug fix is correct.** The
`then(||)` lazy evaluation preserves the atomic try-claim semantics
70921112 introduced. No drift between the doc comment
("Lazy construction is load-bearing") and the implementation.

### 2. ConsumerRunningGuard atomic semantics — `HashSet::insert`

**Files:** `replication_ops.rs:332-366`,
`context.rs:432-435`.

`try_mark_consumer_running` in context.rs:

```rust
pub fn try_mark_consumer_running(&mut self, app_id: &str) -> bool {
    self.running_consumers.insert(app_id.to_string())
}
```

`HashSet::insert(value: T) -> bool` returns `true` if the set did not
previously contain the value, `false` otherwise. This is the CAS
primitive: a single mutation that both checks and sets atomically (in
the sense relevant here — a single non-interleaved call on a
RefCell-protected HashSet in a single-threaded compio runtime).

The brief asks: "verified race-free with the lazy then?" Yes. The
two operations `(check existence, insert if not present)` are a single
`HashSet::insert` call — no observable gap. Even if the runtime
preempted between two consecutive `try_mark_consumer_running` calls,
each call's check-and-set is atomic relative to itself.

Combined with the lazy `then(||)`:

- Winner: `won = true`, `Self` constructed once, Drop fires once on
  exit.
- Loser: `won = false`, `Self` never constructed, no Drop fires
  spuriously.

**The semantics are: "Whichever task's `try_mark_consumer_running`
returns true is the SOLE owner of the marker until that task's guard
drops."** Sound.

**Status:** **Atomic semantics verified.** The
`try_mark_consumer_running` + lazy `then(||)` pair is the canonical
CAS-and-guard idiom in Rust.

### 3. Drop on panic — `guard_drop_unmarks_on_panic_unwind`

**File:** `replication_ops.rs:397-411`.

The test:

```rust
#[test]
fn consumer_running_guard_drop_unmarks_on_panic_unwind() {
    reset_consumer_registry("guard_t3");
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _g = ConsumerRunningGuard::try_claim("guard_t3".into()).expect("first claim wins");
        assert!(context::with(|c| c.is_consumer_running("guard_t3")));
        panic!("simulated supervisor panic mid-loop");
    }));
    assert!(result.is_err(), "the closure should have panicked");
    assert!(
        !context::with(|c| c.is_consumer_running("guard_t3")),
        "Drop must run on panic-unwind and clear the running marker"
    );
}
```

This pins Scenario B from r7 §1 (panic inside `run_supervised`):
panic unwinds through the spawned future, `_guard` drops on the way
out, `unmark_consumer_running` clears the slot. The test exercises
this directly. **Closes a gap in observable test coverage** that
r6/r7 had to argue by code-walk.

`AssertUnwindSafe` is correct here — the test deliberately re-enters
the thread-local context after the panic; the panic is contained.

**Status:** **Drop-on-panic invariant directly tested.** No drift.

### 4. `PENDING_EMITS` flush timing — fresh walk

**Files:** `exec.rs:189-306`, `v8_classes/transaction.rs:117-271`,
`orchestrator/auto_tx.rs:229-275`, `orchestrator/transaction.rs:113-173`.

**Push path (`exec.rs:257-280`):** unchanged from r7. `has_tx` read
borrow, then `push_pending_emit` write borrow — both scope-bounded;
single-thread runtime makes the check-then-push non-racy.

**Drain on COMMIT (`exec.rs:285-298`):** unchanged. `drain_pending_emits`
takes the Option<Vec<_>>; loop runs OUTSIDE the borrow so `emit_local`
calls (which take `borrow_mut` on the broker) can't re-enter.

**Settle path ordering (transaction.rs:253-268):** unchanged. Drain
runs AFTER (1) COMMIT SQL await resolves Ok, (2) Client is dropped
into the spawned connection task. Subscribers cannot observe
pre-commit state.

**Auto-tx settle (auto_tx.rs:245-275):** structurally identical to
v8_classes/transaction.rs. Same visibility ordering. The auto_tx
path also stamps `set_auto_tx_owned(false)` before any await
(line 247) — preserves the "defensive ownership clear before await"
discipline.

**Begin path defensive clear (`orchestrator/transaction.rs:168-171`,
`orchestrator/auto_tx.rs:225`):**

```rust
// Defensive: any residue from a prior tx that didn't drain cleanly
// (shouldn't happen — every settle path clears) must NOT leak into
// the new tx's drain. Drop without firing.
clear_pending_emits();
```

Bounds the **r6 §3d IMPORTANT carry-over** — `run_sql` cancellation
between `take_tx_client` and `put_tx_client` (exec.rs:51-55) drops
the tx client but leaves `pending_emits` queued. The next begin's
defensive clear handles it.

**Transaction Drop finalizer (`v8_classes/transaction.rs:117-138`):**

```rust
fn drop(&mut self) {
    let token = self.token.get();
    if token == 0 || self.settled.get() {
        return;       // Already settled (commit/rollback ran); no clear needed.
    }
    // ... live owner path: drop the client (auto-rollback), then:
    crate::exec::clear_pending_emits();
}
```

The Drop path clears pending_emits **only when the wrapper is the
live owner**. If `settled = true` (e.g., set by `end()`'s
client-already-cleared early-out at line 241 without the drain/clear
running), Drop short-circuits at line 119 and **does NOT call
clear_pending_emits**. The residue then persists until the next
`exec_begin` defensive clear.

This is the **r6 §3d carry-over** unchanged. The leak is bounded but
the invariant is fragile.

**Status:** **`PENDING_EMITS` flush timing unchanged from r7.** The
two IMPORTANTs (`run_sql` cancellation residue + `exec_commit_batch`
is_done window) remain. No new commits this cycle touch this rail.

### 5. `OrchestratorLockGuard::release()` cosmetic Result

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:144-183`.

The brief flags: "release() returns Result<_, DbError> but is
infallible (warn-swallowed unlock errors). Cosmetic."

Walked the body:

```rust
pub(crate) async fn release(mut self) -> Result<Option<PooledClient<'p>>, DbError> {
    if self.released {
        return Ok(self.client.take());      // <-- Ok
    }
    if let Some(client) = self.client.as_ref() {
        let unlock_sql = "SELECT pg_advisory_unlock(hashtext($1)::int4, hashtext($2)::int4)";
        if let Err(e) = client.query_text_params(unlock_sql, &[self.key.as_str(), self.tag]).await {
            tracing::warn!(...);             // <-- swallowed
        }
    }
    self.released = true;
    Ok(self.client.take())                  // <-- Ok
}
```

Confirmed: the function has **no path that returns Err**. The unlock
SQL error path warns and continues. The DbError variant in the
signature exists for future-proofing (so a caller can later
distinguish "this lock couldn't be unlocked" from "this lock was
already released") but is unused today.

Cross-call sites (all three call sites use `let _ = ...`):

```
$ rg -n 'lock_guard.release' crates/plugin-db/src/
crates/plugin-db/src/orchestrator/register_model/apply.rs:226:    let _ = lock_guard.release().await;
crates/plugin-db/src/orchestrator/register_model/bootstrap.rs:137:            let _ = guard.release().await;
crates/plugin-db/src/orchestrator/register_model/mod.rs:219:            let _ = lock_guard.release().await;
```

The `let _ =` pattern at call sites would be a finding if `release()`
could actually return Err. Today it can't, so the call sites are
consistent with the function's actual behaviour.

**Classification:** cosmetic / future-proofing only. **Not a
finding.** Two minor possibilities:

- Tighten the signature to `pub(crate) async fn release(mut self) ->
  Option<PooledClient<'p>>` (drop the Result wrapper); the call sites
  become `let _client = lock_guard.release().await;`.
- Keep the Result for future use but document the current
  infallibility.

Either is a docstring-level change. **Not blocking.**

**Status:** **Cosmetic only.** As the brief calls it.

### 6. WAL consumer task lifecycle — `running_consumers` slot + Drop

**File:** `replication_ops.rs:189-313` (dispatch) +
`replication_ops.rs:332-366` (guard) + `wal_consumer.rs:138-159`
(SuppressGuard).

The current shape:

```rust
// Outer dispatch body (inside spawned_ops):
let already = crate::context::with(|c| c.is_consumer_running(&app_id));
if already { return ShortCircuitEnvelope; }   // r7 idempotent gate
// ... ensure_pool, ensure_publication_and_slot, build consumer ...
let app_for_task = app_id.clone();
compio::runtime::spawn(async move {
    let Some(_guard) = ConsumerRunningGuard::try_claim(app_for_task) else {
        return;            // <-- atomic loser path: bail without mark/spawn
    };
    crate::wal_consumer::run_supervised(consumer).await;
    // _guard drops here on every exit (Ok, Err, panic, future-dropped).
}).detach();
```

**Walked every lifecycle scenario:**

- **A. Future dropped pre-poll.** `_guard` never constructed (the
  `let Some(_guard) = ...` line never executed). `try_mark_consumer_running`
  never called. No mark, no Drop. State coherent. Correct.

- **B. Race between dispatch return and first poll.** The r7 finding
  scenario: two rapid-succession dispatches both pass the outer
  `is_consumer_running` gate at line 199 before either has marked.
  Both reach the spawn. Their futures both poll `try_claim`. Whichever
  runs `try_mark_consumer_running` first wins (`won = true`); the
  other gets `won = false`, `try_claim` returns None, the loser
  returns without spawning a consumer. **The race window is now
  closed BY CONSTRUCTION.** Postgres never sees a second
  START_REPLICATION on the same slot; no infinite retry-with-backoff.
  Closes r7 MINOR-R7-1.

- **C. Panic inside `run_supervised`.** Unwind drops `_guard`;
  `unmark_consumer_running` clears the slot. Test
  `guard_drop_unmarks_on_panic_unwind` pins this directly.

- **D. Graceful CopyDone.** `run_supervised` returns Ok; `_guard`
  drops on function exit; slot cleared.

- **E. Fatal Err from is_fatal.** `run_supervised` returns; `_guard`
  drops; slot cleared.

**SuppressGuard ⊂ ConsumerRunningGuard nesting:** unchanged from r7.
SuppressGuard is per-`run()` (inside the supervisor loop), nested
inside the outer ConsumerRunningGuard. On graceful exit, inner drops
first (LIFO unwind). No interference.

**Production-only API surface (`e5315083`):** `mark_consumer_running`
(the non-atomic insert) is gated behind `#[cfg(any(test, feature =
"test-helpers"))]` at `context.rs:423-426`. Production code can ONLY
call `try_mark_consumer_running`. Eliminates the api-surface footgun
where a contributor could pick the non-atomic variant by name
similarity.

**Status:** **`r7 MINOR-R7-1 closed.`** The atomic try-claim
inside the spawned future closes the race window absolutely. All five
lifecycle scenarios leave the slot in a defined state.

### 7. `broker.rs publish()` borrow_mut across `waker.wake()` — latent

**File:** `broker.rs:306-328`, `480-528`.

Unchanged since r6/r7. `Subscription::push` holds `inner =
self.0.borrow_mut()` across `w.wake()`:

```rust
pub fn push(&self, msg: SubscriptionMessage) {
    let mut inner = self.0.borrow_mut();
    if inner.closed { return; }
    if inner.queue.len() >= inner.max_queue {
        // ... overflow path
        if let Some(w) = inner.waker.take() {
            w.wake();         // <-- wake while holding inner
        }
        return;
    }
    inner.queue.push_back(msg);
    if let Some(w) = inner.waker.take() {
        w.wake();             // <-- wake while holding inner
    }
}
```

`Waker::wake` (compio): enqueues the task into the runqueue without
synchronously polling. Re-entry into the same `RefCell` is not
possible under compio's current scheduler. **Safe today**, **latent
if a future Waker shim invokes synchronously**.

**Status:** **MINOR, latent, carry-over.** No code change since r6.
Same disposition as r7 §6.

### 8. `TX_CONN` ownership — fresh re-walk

Grepped for every mutator:

```
$ rg -n 'install_tx_client|take_tx_client|put_tx_client' crates/plugin-db/src/
  context.rs:265,273,278  (definitions)
  exec.rs:51, 55          (run_sql round-trip)
  v8_classes/transaction.rs:130, 237  (Drop finalizer + end())
  orchestrator/transaction.rs:165     (begin)
  orchestrator/auto_tx.rs:218, 246    (exec_auto_begin + exec_auto_end)
```

**Same six call sites as r6/r7.** No new mutation paths since r6.

Pairing audit:

| Operation | Site | Pairs with |
| --- | --- | --- |
| `install_tx_client` | `orchestrator/transaction.rs:165` | `take_tx_client` at `v8_classes/transaction.rs:237` (end) or `:130` (Drop). |
| `install_tx_client` | `orchestrator/auto_tx.rs:218` | `take_tx_client` at `orchestrator/auto_tx.rs:246` (exec_auto_end). |
| `take_tx_client` | `exec.rs:51` (run_sql) | `put_tx_client` at `exec.rs:55`. |
| `take_tx_client` | `v8_classes/transaction.rs:130` (Drop) | terminal (Drop owns the take). |
| `take_tx_client` | `v8_classes/transaction.rs:237` (end) | terminal. |
| `take_tx_client` | `orchestrator/auto_tx.rs:246` (exec_auto_end) | terminal. |
| `put_tx_client` | `exec.rs:55` | re-installs after run_sql take. |

run_sql's take/put pair never crosses a settle boundary (compio
single-thread runtime). The token ownership pattern (`set_tx_token` /
`auto_tx_owned`) is enforced by the `debug_assert!`s at
`context.rs:294-298, 324-329`.

**Status:** **TX_CONN ownership unchanged from r7.** No new mutation
paths. Invariants intact.

---

## New findings this round

**None.**

---

## Findings, structured per brief

For each retained or closed finding, the brief's requested structure:

### CLOSED — r7 NEW MINOR-R7-1 — race window between dispatch return and first poll of spawned consumer

```
[CLOSED] crates/plugin-db/src/replication_ops.rs:287-297
  Why: r7 scenario — two rapid-succession dispatches both pass the
       outer is_consumer_running gate before either marks; second
       could spawn a duplicate consumer.
  Fix landed: 70921112 introduced `try_mark_consumer_running`
       (atomic HashSet::insert returning won bit); 386f9bf5 then
       lifted ConsumerRunningGuard to module scope with `try_claim`
       calling try_mark + lazy `then(||)`. Loser path: try_claim
       returns None inside the spawned future; the spawned task
       early-returns without provisioning. Window closed by
       construction — Postgres never sees a duplicate START_REPLICATION.
  Repro (pre-fix): two startReplicationConsumer() in flight; second
       spawn observes is_consumer_running=false; both build consumer;
       Postgres rejects second with SQLSTATE 55006 (object_in_use);
       is_fatal does NOT mark 55006 fatal → infinite retry loop.
  Verification: replication_ops.rs:289 (try_claim guard inside
       spawned future); replication_ops.rs:356-359 (lazy then);
       replication_ops.rs:414-424 (try_claim_loses test pins
       semantics); context.rs:432-435 (HashSet::insert atomic).
```

### CARRY-OVER IMPORTANT — `run_sql` cancellation drops `tx_conn` but leaves `pending_emits` queued

```
[IMPORTANT] crates/plugin-db/src/exec.rs:51-55 +
            crates/plugin-db/src/v8_classes/transaction.rs:237-244
  Why: run_sql takes tx_conn out across an .await; cancellation
       between :51 (take) and :55 (put) drops the Client (PG
       observes torn sender → server-side ROLLBACK). pending_emits
       queued earlier in the same tx remain. Transaction::end's
       client_opt=None early-out at :238-244 sets settled=true and
       returns Ok WITHOUT calling drain_pending_emits_on_commit OR
       clear_pending_emits. The Drop finalizer at :117-138 sees
       settled=true and returns at :119 — also skipping the clear at
       :137. Residue persists until next exec_begin's defensive
       clear (orchestrator/transaction.rs:171).
  Repro: tx active with N writes (each push_pending_emit); a JS
       Promise driving run_sql is cancelled mid-await; user code
       never explicitly settles the tx; an unrelated subsequent
       handler kicks off another tx → defensive clear runs. Between
       the cancellation and the next tx, the residue sits in the
       per-isolate slot. Memory leak (small) + theoretical correctness
       risk if a NEW broker.publish path is added that re-enters the
       slot before exec_begin.
  Fix: in Transaction::end's client-already-cleared branch
       (transaction.rs:238-244) and Transaction::drop's settled-true
       early-out (transaction.rs:119), call clear_pending_emits()
       unconditionally before returning. One-liner each site. Same
       discipline as exec_begin's defensive clear.
  Verification: exec.rs:43-73 (run_sql take/put);
       v8_classes/transaction.rs:117-138 (Drop early-out);
       v8_classes/transaction.rs:237-244 (end early-out);
       orchestrator/transaction.rs:168-171 (defensive clear bounds
       the leak).
```

### CARRY-OVER IMPORTANT — `exec_commit_batch` is_done path's `release_advisory_lock` cancellation window

```
[IMPORTANT] crates/plugin-db/src/migrations.rs:638-657
  Why: in the is_done terminal branch, finalise_backfill runs
       (warn-on-err per 51ced4a0), then release_advisory_lock
       awaits at :654, then drop(client) at :656, then
       clear_mig_lock at :657. If the future is cancelled between
       :654's await and :657's clear, mig_lock stays set with
       client=None (it was dropped). Future migrationBegin calls
       see has_mig_lock=true and reject with concurrent_migration
       until the operator runs reset().
  Repro: migration on path A reaches is_done=true; JS Promise
       cancelled by V8 (e.g., isolate shutdown signal) between
       :654 await resolving and :657's slot clear. Server-side
       advisory lock release is observable (the .await returned),
       but the in-process mig_lock slot still says "Running".
  Fix: same family as ConsumerRunningGuard. RAII guard for the
       mig_lock slot: on Drop, call clear_mig_lock. Construct at
       set_mig_lock site; release via .into_persistent() when the
       caller intentionally keeps the lock across an await chain
       that doesn't own the cleanup. Drop fires on cancel/panic
       regardless.
  Verification: migrations.rs:617-658 (is_done branch);
       context.rs:362-372 (set_mig_lock / clear_mig_lock).
```

### CARRY-OVER MINOR — `apply.rs` `update_audit_status` Err swallow

```
[MINOR] crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186
  Why: success and failure branches both call update_audit_status
       via `let _ = backend.update_audit_status(...)`. If the audit
       UPDATE fails after the DDL succeeded, the audit row stays in
       Running with no operator-visible signal. Sibling of the
       finalise_backfill case 51ced4a0 closed in migrations.rs:638.
  Fix: same one-line shape as 51ced4a0:
         if let Err(e) = backend.update_audit_status(...).await {
             tracing::warn!(app_id=%app_id, audit_id=id,
                 error=%e, "update_audit_status failed; audit row
                 may stay Running until next reset()");
         }
       Apply twice (success branch at :163, failure branch at :178).
  Repro: a backend hiccup between DDL completion and audit UPDATE.
       The DDL is durable, the lock is released, but operators
       querying __zeroship_migrations.status see "Running" for what
       is a completed migration.
  Verification: apply.rs:160-188 (both branches use let _ today);
       migrations.rs:638-651 (the F1-pattern reference applied
       successfully in 51ced4a0).
```

### CARRY-OVER MINOR — broker `publish` borrow_mut spans `waker.wake()`

```
[MINOR] crates/plugin-db/src/broker.rs:306-328
  Why: Subscription::push holds `inner = self.0.borrow_mut()`
       across w.wake(). Compio's Waker::wake enqueues without
       synchronously polling, so re-entry is impossible TODAY.
       Latent if a future Waker shim invokes the task
       synchronously (e.g., a thread-local executor used in tests).
  Fix: drop `inner` before w.wake():
         let waker = inner.waker.take();
         drop(inner);          // release the borrow_mut
         if let Some(w) = waker { w.wake(); }
       Two sites: overflow path (:319-321) and normal push (:325-327).
  Repro: not exploitable under current compio scheduler. Would
       become exploitable if a future Waker variant invokes
       synchronously and the woken task re-enters subscription
       state on the same thread.
  Verification: broker.rs:306-328 (push); broker.rs:480-529 (publish
       — note: publish itself doesn't hold a borrow across wake; the
       hazard is inside push). r6/r7 unchanged disposition.
```

### CARRY-OVER MINOR — CIC retry budget × pool depth (transient saturation)

```
[MINOR] crates/plugin-db/src/backend/postgres.rs:385-600
  Why: MAX_RETRIES = 3 caps the retry budget; each retry holds a
       brief pool checkout. N concurrent register_model calls all
       hitting CIC for distinct indexes can transiently saturate the
       pool (default 16). Not unbounded growth; transient.
  Fix: not required — re-classified r7 as transient saturation, not
       unbounded.
  Repro: 4+ concurrent register_model calls on a fresh worker with
       default pool size 16, each retrying CIC. Brief pool starvation
       observable as latency spikes on unrelated DB ops.
  Verification: backend/postgres.rs:396 (MAX_RETRIES=3);
       backend/postgres.rs:455 (pool checkout per attempt).
```

### COSMETIC — `OrchestratorLockGuard::release()` returns `Result<_, DbError>` but is infallible

```
[COSMETIC] crates/plugin-db/src/orchestrator/lock_guard.rs:144-183
  Why: the unlock-SQL Err path warns-and-continues internally
       (lines 168-179); release() always reaches the trailing
       `Ok(self.client.take())`. All three call sites use `let _ =
       guard.release().await` — consistent with the function's
       actual behaviour.
  Fix: either
       (a) tighten the signature to `async fn release(mut self) ->
           Option<PooledClient<'p>>` (drop the Result wrapper); or
       (b) keep the Result for future-proofing but add a docstring
           note: "Today release() is infallible; the Err arm is
           reserved for future telemetry where a caller may need to
           distinguish unlock-failed from already-released."
  Repro: N/A (no observable behaviour to break).
  Verification: lock_guard.rs:144-183 (function body has no
       early Err return); all three call sites use `let _`
       (apply.rs:226, bootstrap.rs:137, mod.rs:219).
```

### DESIGN — `OrchestratorLockGuard::Drop` log-only contract

Unchanged. Drop can't await; production code reaches `release()` or
`into_held()`. The session-scoped pg_advisory_lock auto-releases on
backend session close. Documented contract.

---

## Invariants that held up under r8 audit

- **`386f9bf5` latent bug fix.** `won.then(|| Self { app_id })` lazy
  construction is correct; the lost-race path never constructs Self,
  never fires Drop, never spuriously unmarks the winner. Direct test
  pins this (`try_claim_loses_when_already_marked`).
- **`e5315083` API gating.** Non-atomic `mark_consumer_running` is
  gated behind `cfg(any(test, feature = "test-helpers"))`; production
  code can ONLY call the atomic `try_mark_consumer_running`.
  Eliminates the api-surface footgun.
- **r7 NEW MINOR-R7-1 closed.** Atomic try-claim inside the spawned
  future closes the dispatch-return-vs-first-poll race window by
  construction.
- **All five ConsumerRunningGuard lifecycle scenarios.** (A)
  future-dropped-pre-poll, (B) race-between-spawns, (C)
  panic-in-loop, (D) graceful-CopyDone, (E) fatal-Err. Each leaves
  the slot in a defined state. Direct tests pin (B/C/D); (A/E)
  follow from RAII semantics.
- **SuppressGuard ⊂ ConsumerRunningGuard lifetime nesting.** Inner
  guard per `consumer.run()` invocation; outer guard for the
  spawned task's whole life. LIFO unwind order on panic. No
  interference. Documented "suppression-off during backoff" gap is
  intentional.
- **OrchestratorLockGuard 4-layer hardening.** All four design-pass
  commits (`cbd12944`, `bd1e7ce1`, `808a32af`, `ffb1e101`) still
  cross-walked against the source; no drift. The release() Result
  is cosmetic (acknowledged).
- **PENDING_EMITS visibility ordering.** `drain_pending_emits_on_commit`
  fires after COMMIT `.await` resolves AND after the client is
  dropped. `clear_pending_emits` defensively wipes residue at the
  start of every `exec_begin`.
- **TX_CONN ownership.** Same six call sites as r6/r7; every begin
  pairs with a settle; run_sql's take/put never crosses a settle
  boundary (compio single-thread).
- **Compio task scheduling assumption** (single-thread per isolate)
  unchanged. Every plugin-db spawn captures `!Send` data; the
  runtime can't migrate them across threads.
- **`deeefe18` coded_db dedup** is concurrency-neutral. Same
  error-rail surface, no new RefCell-across-await.
- **`f6043126` SQLSTATE-typed checks** is concurrency-neutral.
  Replaces substring matching on error messages; no async ordering
  change.

---

## Score: **88 / 100** (+2 vs r7)

Why the score went up:

- **One r7 finding closed by `70921112` + `386f9bf5`.** The
  NEW MINOR-R7-1 race window between dispatch return and first poll
  is closed BY CONSTRUCTION inside the spawned future. The atomic
  try-claim eliminates the window without depending on compio
  scheduler discipline.
- **The latent bug fix in `386f9bf5` is the most significant
  correctness improvement since r7.** Pre-fix, the `then_some`
  formulation would have silently broken 70921112's atomic
  semantics on every contended race. The new test suite catches
  this at unit-test time — including a structural test for [I42]
  in `lock_guard.rs` that pins source-order invariants.
- **`e5315083`'s API surface gating** eliminates a forward-looking
  footgun. A contributor browsing context.rs cannot accidentally
  pick the non-atomic variant in production.
- **No regressions.** `deeefe18` and `f6043126` are
  concurrency-neutral.

Why the score isn't higher:

- **Two IMPORTANTs remain** (`run_sql` cancellation `pending_emits`
  residue; `exec_commit_batch` is_done `release_advisory_lock`
  cancellation window). Same RAII-guard-for-context-slot fix family.
  Each is a small bounded fix. These are now the gap to 92+.
- **The apply.rs `update_audit_status` MINOR** remains —
  one-line `if let Err(e) = ... tracing::warn!` would close it.
  Sibling of the migrations.rs case 51ced4a0 already closed.
- **`OrchestratorLockGuard::release()` cosmetic Result** is
  noise-level — tighten the signature or document the
  infallibility.
- **Broker `publish` waker.wake() borrow** remains MINOR-latent;
  safe under compio's current Waker discipline.

Why not higher movement (capped at +2):

- Closing one MINOR + applying one correctness fix that prevents
  a regression in an already-shipped atomic semantic is a net
  improvement, but the two IMPORTANTs still dominate the gap. Their
  combined surface area is now the bulk of the 12-point deficit.
- The bug `386f9bf5` caught WAS landed in production briefly
  (between 70921112 and 386f9bf5, the `then_some` form was in
  HEAD). It was not in a released build (the test surfaced it
  before merge, per the commit message). No retrospective score
  adjustment.

| Round | Score | CRITICALs | IMPORTANTs | New findings |
| ----- | ----- | --------- | ---------- | ------------ |
| r2    | 82    | 0         | 4          | several      |
| r3    | 73    | 1 (boot)  | 4          | several      |
| r4    | 80    | 0         | 4          | 1 MINOR      |
| r5    | 80    | 0         | 5          | 1 MINOR      |
| r6    | 84    | 0         | 3 (2 closed) | 0          |
| r7    | 86    | 0         | 2 (1 closed) | 1 MINOR    |
| r8    | **88** | 0       | **2** (carry) | **0** (1 r7-MINOR closed) |

---

## Closing summary

R8 confirms that `386f9bf5` correctly fixes the latent bug in the
atomic try-claim's `then_some` formulation; the new lifecycle test
suite (4 tests) directly exercises every ConsumerRunningGuard exit
path including panic-unwind. `e5315083` closes the api-surface
footgun by gating the non-atomic `mark_consumer_running` behind
test-only `cfg`. R7's NEW MINOR-R7-1 race window is **closed by
construction** by the atomic try-claim inside the spawned future.

`deeefe18` and `f6043126` are concurrency-neutral —
error-rail-discipline and SQLSTATE-fragility fixes respectively,
neither changes async ordering, RefCell discipline, or shared state.

The remaining gap to 92+ is the two `run_sql`/`release_advisory_lock`
cancellation IMPORTANTs — same RAII-guard-for-context-slot fix
family the ConsumerRunningGuard pattern (70921112+386f9bf5)
demonstrates works. Each is a one-type-plus-three-call-site fix.

**Score: 88 / 100** (r7: 86 / 100). +2 reflects one MINOR closed by
construction + one correctness improvement (latent-bug-fix +
api-surface tightening), with no regressions.
