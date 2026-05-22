# plugin-db concurrency / lifecycle review — 2026-05-22 r10

**Commit:** `d2e7e22` (HEAD; post-r9 cycle 09:47 + cycle 10:17 backlog audit)
**Lens:** concurrency + lifecycle (round 10, fresh re-audit)
**Prior rounds:** `…-r9.md` (88/100), `…-r8.md` (88), `…-r7.md` (86),
`…-r6.md` (84), `…-r5.md` (80), `…-r4.md` (80), `…-r3.md` (73),
`…-r2.md` (82).

**Commits since r9 (in chronological order):**

- `7d0bc4c5` — `exec.rs:64,317` unified cold-init code to
  `lazy_init_failed`. SDK contract / error-rail only. **Audited in r9.**
- `389749ca` — `v8_classes/migration.rs:269` +
  `v8_classes/migrations.rs:180` unified `not_configured` →
  `backend_not_initialized` for the `context.backend() == None`
  branch. Twin of `7d0bc4c5`. Pure `&'static str` constant rename
  inside two `Err(...)` arms; no surrounding control-flow change.
- `7bd2187e` — `crates/plugin-db/benches/bench_query_build.rs`
  scaffold (167 lines added). Criterion bench harness; entirely
  outside `src/`. No production-code change.
- `757026e3` — Doc-only fixes in `audit.rs:5`, `error.rs:28-34`,
  `orchestrator/mod.rs:22`, `query.rs:443`. Comments only;
  re-confirmed via `git show`.
- `bed655c1` — Doc-only fix in `replication.rs:744-754` (test
  docstring drift on `empty_returning_string_shape_keeps_…`).
  Comments only.
- `a6dca645` — Backlog audit; only writes to `docs/reviews/` and
  `docs/backlog/`. No code touched.

**Audit dimensions (per brief):**

1. Carry-over IMPORTANTs/MINORs from r6/r7/r8/r9.
2. `broker.rs publish()` borrow_mut across `waker.wake()` — latent
   flag.
3. Plateau / asymptote check.

---

## Headline

**No code change in this window touches a synchronisation path. All
three carry-over findings persist verbatim. One NEW MINOR-latent
finding surfaces this round: `Subscription::close` (broker.rs:359-369)
holds `inner = self.0.borrow_mut()` across `w.wake()`** — same class
as the previously-noted `Subscription::push` MINOR-latent at
broker.rs:319-321, but at a *third site* that rounds r6/r7/r8/r9 all
missed. Safe under current compio Waker semantics, latent if a future
synchronous-poll Waker shim lands.

Score holds at **88 / 100** — the new finding is MINOR-latent (no
runtime hazard today) and ranks below the two IMPORTANTs as the gating
constraint on the headline. It nudges the post-landing ceiling
slightly (95.5 instead of 95 once everything carry-over lands), not
the current floor.

---

## Audit dimensions, walked fresh

### 1. Recent commits — synchronisation surface

**`389749ca` — `not_configured` → `backend_not_initialized` rename.**

Two-call-site diff:

```diff
-DbError::config("not_configured", "db: backend not initialised")
+DbError::config("backend_not_initialized", "db: backend not initialized")
```

Both sites are inside `crate::context::with(|c| c.backend()).ok_or_else(...)`
chains. The `with` call takes a `&` borrow on the isolate context
(read-only), runs the closure synchronously, drops the borrow before
the `.ok_or_else` arm fires. The closure is sync. No await sequencing
involved. **Zero synchronisation surface.**

**`7bd2187e` — bench scaffold.** New file
`crates/plugin-db/benches/bench_query_build.rs` (167 lines) + 11 LOC
in `Cargo.toml`. Touches `build_find` / `build_insert` (pure functions
that build SQL strings — no DB, no async, no shared state). Confirmed
no `src/*.rs` change. **Zero synchronisation surface.**

**`757026e3`, `bed655c1`, `a6dca645`** — confirmed via `git show`
that every emitted hunk is a `//`/`///` comment or a `docs/**` file.
**Zero synchronisation surface.**

**Verdict:** Same conclusion as r9 for `7d0bc4c5`. All commits in
this window are concurrency-neutral by construction.

### 2. r6/r7/r8/r9 carry-overs — re-walked

#### 2a. `run_sql` cancellation drops `tx_conn` but leaves `pending_emits` queued — UNCHANGED

**Files re-walked at HEAD:** `exec.rs:43-73` (run_sql take/put pair);
`v8_classes/transaction.rs:119-141` (Drop early-out at `:122`);
`v8_classes/transaction.rs:224-273` (end early-out at `:240-246`,
normal settle at `:266-270`); `orchestrator/transaction.rs:165-173`
(defensive clear at `:172` that bounds the leak).

**`run_sql` body** (lines 43-73):

```rust
pub(crate) async fn run_sql(
    sql: &str,
    params: &[&str],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    let has_tx = context::with(|c| c.has_tx());
    if has_tx {
        let client = context::with_mut(|c| c.take_tx_client())     // :51
            .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
        let result = client.query_text_params(sql, params).await;  // :53
        context::with_mut(|c| c.put_tx_client(client));            // :55
        return result.map_err(|e| DbError::from_pg(&e));
    }
    // ...
}
```

**`Transaction::drop`** (lines 119-141):

```rust
fn drop(&mut self) {
    let token = self.token.get();
    if token == 0 || self.settled.get() {
        return;                                  // <-- short-circuit; NO clear_pending_emits
    }
    let current = crate::context::with(|c| c.tx_token());
    if current != token {
        return;
    }
    // Live owner path: drop(client) + clear_pending_emits() at :139
    let client: Option<Client> = crate::context::with_mut(|c| {
        let client = c.take_tx_client();
        c.set_tx_token(0);
        client
    });
    drop(client);
    crate::exec::clear_pending_emits();          // <-- only on live-owner path
}
```

**`end()` early-out** (lines 240-246):

```rust
let client_opt = crate::context::with_mut(|c| c.take_tx_client());
let Some(client) = client_opt else {
    // tx_conn already cleared by another path — treat as already
    // settled rather than a fresh failure.
    this.settled.set(true);
    crate::context::with_mut(|c| c.set_tx_token(0));
    return Ok(());                               // <-- NO clear_pending_emits before return
};
```

**Normal `end()` settle** (lines 266-270):

```rust
if cmd == "COMMIT" && result.is_ok() {
    crate::exec::drain_pending_emits_on_commit();
} else {
    crate::exec::clear_pending_emits();
}
```

So the residue path is: `run_sql` cancellation drops `Client` (PG
auto-rollback server-side); subsequent `end()` hits the
`client_opt = None` early-out at `:240-246` and returns Ok without
clearing; the wrapper has `settled = true` (set at `:243`); the GC
finalizer at `:119-122` then short-circuits without reaching the
live-owner path's `clear_pending_emits` at `:139`. Residue persists
until the next `exec_begin`'s defensive clear at `orchestrator/
transaction.rs:172`.

**Bounded but fragile**, as r6/r7/r8/r9 noted. Status: **carry-over
IMPORTANT, unchanged**. No commit in the r9 → r10 window touched the
rail.

Fix (same as r6/r7/r8/r9): in both `Transaction::end`'s
`client_opt = None` early-out (`transaction.rs:240-246`) and
`Transaction::drop`'s `settled = true` short-circuit
(`transaction.rs:119-123`), call `crate::exec::clear_pending_emits()`
unconditionally before returning. One-liner each site.

#### 2b. `exec_commit_batch` is_done window — `release_advisory_lock` cancellation — UNCHANGED

**File re-walked at HEAD:** `migrations.rs:602-656`.

The `is_done` terminal branch (lines 614-656) is byte-identical to
r9's reading. Sequence:

```rust
if is_done {
    let terminal = match terminal_status.unwrap_or("applied") { ... };

    if let Err(e) = backend
        .finalise_backfill(&client, app_id, audit_id, terminal, error_message)
        .await                                       // <-- :637 await
    {
        tracing::warn!(...);                         // 51ced4a0 closed this
    }

    let lock_key = format!("zs_mig:{app_id}");
    backend.release_advisory_lock(&client, &lock_key, &name).await;  // <-- :651 await
    drop(client);                                                    // <-- :653
    crate::context::with_mut(|c| c.clear_mig_lock());                // <-- :654
    return Ok(serde_json::json!({ "committed": !dry_run, "done": true }).to_string());
}
```

Cancellation between `:651` and `:654` leaves the in-process
`mig_lock` slot in `Running` even though Postgres has released the
server-side advisory lock (either via the SQL completing or via the
session close from `drop(client)`). The next `migrationBegin` sees
`has_mig_lock = true` at `migrations.rs:228` and rejects with
`migration_already_active` until the operator runs `reset()`.

**Note on the *other* release_advisory_lock site at
`migrations.rs:287-291`:** I checked whether the cancelled-row
rejection path in `exec_begin` has the same hazard. **It does not.**
`set_mig_lock` is only called at `migrations.rs:329` — *after* the
cancelled-row branch returns. So at `:287-291`, the in-process slot
has never been set; cancellation between `:289 await` and
`:290 drop(client)` is safe (PG auto-releases on session close, no
in-process residue).

The hazard is exclusively in the **`is_done` terminal branch**.

Status: **carry-over IMPORTANT, unchanged**. Same fix family as
`ConsumerRunningGuard` (70921112+386f9bf5): a `MigLockGuard` RAII type
that owns the per-isolate `mig_lock` slot and calls `clear_mig_lock`
on Drop. Construct at the `set_mig_lock` site at `:328-336`; release
explicitly via `.into_persistent()` (or analogue) if a future caller
intentionally hands ownership across an await chain not owned by
cleanup. Drop fires on cancel/panic regardless of where in
`:651 → :654` the future unwound.

#### 2c. `apply.rs update_audit_status` Err swallow — UNCHANGED

**File re-walked at HEAD:** `orchestrator/register_model/apply.rs:160-188`.

Byte-identical to r9: both Ok branch (`:163-171`) and Err branch
(`:178-185`) use `let _ = backend.update_audit_status(...).await;`.

```rust
if let Some(id) = audit_id {
    match &result {
        Ok(_) => {
            let _ = backend
                .update_audit_status(&app_id, id,
                    crate::audit::TerminalStatus::Applied, None)
                .await;
        }
        Err(e) => {
            let msg = e.clone().into_string();
            let _ = backend
                .update_audit_status(&app_id, id,
                    crate::audit::TerminalStatus::Failed,
                    Some(msg.as_str()))
                .await;
        }
    }
}
```

If the audit UPDATE fails after the DDL succeeded, the audit row
stays `Running` with no operator-visible signal. Sibling of the
`finalise_backfill` case `51ced4a0` closed in `migrations.rs:635-648`.

Status: **carry-over MINOR, unchanged**. Same one-line fix per site.

### 3. broker.rs `publish` borrow_mut across `waker.wake()` — latent

**File re-walked at HEAD:** `broker.rs:306-328` (push); `broker.rs:480-529`
(publish); **`broker.rs:357-369` (close) — NEW THIS ROUND.**

#### 3a. `Subscription::push` — carry-over (unchanged from r9)

```rust
pub fn push(&self, msg: SubscriptionMessage) {
    let mut inner = self.0.borrow_mut();        // :307 — held across :320 + :326
    if inner.closed { return; }
    if inner.queue.len() >= inner.max_queue {
        if !inner.resync_pending {
            inner.queue.clear();
            inner.queue.push_back(SubscriptionMessage::Resync);
            inner.resync_pending = true;
        }
        if let Some(w) = inner.waker.take() {
            w.wake();                            // :320 — borrow_mut still live
        }
        return;
    }
    inner.queue.push_back(msg);
    if let Some(w) = inner.waker.take() {
        w.wake();                                // :326 — borrow_mut still live
    }
}
```

**Two latent wake-while-borrowed sites: `:320` and `:326`.**

#### 3b. `Subscription::close` — NEW finding this round

```rust
pub fn close(&self) {
    let mut inner = self.0.borrow_mut();        // :360 — held across :367
    if inner.closed { return; }
    inner.closed = true;
    inner.queue.push_back(SubscriptionMessage::Closed);
    if let Some(w) = inner.waker.take() {
        w.wake();                                // :367 — borrow_mut still live
    }
}
```

**Identical pattern to `push`.** A third latent wake-while-borrowed
site at `:367` that rounds r6/r7/r8/r9 all missed — they enumerated
`push`'s two sites but did not name `close()`.

#### 3c. `Broker::publish` — confirmed safe (unchanged from r9)

Re-walked `:480-529`. `publish` takes `&mut self` and holds
`&mut subs` (the per-collection `Vec<Subscription>`) across the
`for s in subs.iter()` loop at `:510-515`. Inside the loop,
`s.push(...)` borrows the *subscription's* RefCell — a different
RefCell from the broker's HashMap. The `&mut subs` borrow is on a
disjoint piece of memory from the `RefCell<Inner>` inside each
`Subscription`. No re-entry hazard at the broker level.

**The latent hazard is in `Subscription::push` AND
`Subscription::close`, not in `Broker::publish`.**

#### Wake semantics under compio (re-verified)

`Waker::wake` under compio enqueues the woken task to the run queue
without synchronously polling. The woken task cannot re-enter the
same `Subscription`'s `RefCell` because re-entry would require the
current compio thread to poll the woken task *while still inside* the
calling `push`/`close`. That's impossible (the current fiber is the
calling one; the run queue isn't drained until the current fiber
yields).

**Safe today.** Becomes exploitable only if:

- A future Waker shim invokes the wake target synchronously (e.g., a
  test harness using `futures::task::noop_waker_ref` or a direct-poll
  Waker that re-enters), AND
- The woken task re-enters the same Subscription's RefCell
  synchronously (e.g., a callback chain that calls `push` /
  `register_waker` from inside a wake-driven poll).

Both conditions would need to be met for a panic. Under production
compio (which always enqueues), neither can occur.

Status: **MINOR-latent, three sites total** (push overflow,
push normal, close). r9 named two; r10 adds the third. Same fix per
site:

```rust
let waker = inner.waker.take();
drop(inner);                          // release the borrow_mut
if let Some(w) = waker { w.wake(); }
```

### 4. Plateau / asymptote

**Setup.** r9 stated: "without landings, asymptote is 89; with both
IMPORTANTs landed, 92-93." The brief asks me to validate that.

**Walk of the gap (88 → 100) at HEAD:**

| Finding | Severity | Lift if landed | Lift if not |
| --- | --- | --- | --- |
| run_sql cancel pending_emits residue | IMPORTANT | +2 | 0 |
| exec_commit_batch is_done window | IMPORTANT | +2 | 0 |
| apply.rs update_audit_status swallow | MINOR | +1 | 0 |
| broker push waker.wake() borrow ×2 sites | MINOR-latent | +0.5 | 0 |
| broker close waker.wake() borrow ×1 site (NEW r10) | MINOR-latent | +0.5 | 0 |
| CIC retry × pool depth | MINOR-transient | 0 (re-class) | 0 |
| OrchestratorLockGuard::release Result | COSMETIC | +0.5 | 0 |

**Theoretical ceiling without landings:** the new `close()` finding
this round suggests the lens is not fully exhausted — even after 4
rounds (r6/r7/r8/r9) all of which named `push`'s two wake sites, none
caught the third site at `close()`. That's evidence the asymptote is
not 89 in the strict sense: another careful re-walk can still surface
a missed-but-existing latent issue.

But:

- The new finding is **same class** as one already-tracked
  (MINOR-latent broker waker borrow).
- It's **bounded by the same compio Waker semantics** — not
  exploitable today.
- Its lift is +0.5 if landed (paired with the existing `push` fix);
  zero if not.

So the *practical* asymptote-without-landings has moved from 89
(r9's call) to **roughly 88.5-89** — a half-point shaved off by
finding-and-not-landing a same-class issue. The headline score does
not move because the gating constraint remains the two IMPORTANTs.

**Ceiling math, refreshed:**

- No landings: 88 (current floor) → 88.5-89 ceiling. r10 has not
  moved it materially.
- Land both IMPORTANTs (2a + 2b): +4 → **92**.
- Add apply.rs MINOR (2c): +1 → **93**.
- Add ALL broker waker borrow fixes (3a + 3b — should be done
  together): +0.5 → **93.5**.
- Add OrchestratorLockGuard::release Result tightening: +0.5 → **94**.
- Beyond 94 requires either property-test campaign or new sync
  surface introduced by future code.

**The new finding does NOT invalidate the plateau call.** It marginally
expands the surface still discoverable by careful re-walking, but
the gating constraint on the headline is unchanged (two IMPORTANTs).
**The lens has effectively converged for the headline; marginal new
findings are now MINOR-latent of the same class.**

**Diminishing-returns assessment:** r10 surfaces +1 MINOR-latent
relative to r9. r9 surfaced 0. r8 surfaced 0. r7 surfaced 1 MINOR
(closed by 386f9bf5). The signal-to-noise of further re-audits is now
~0.25 findings/round, all MINOR-latent. The next concurrency cycle
should still be scheduled after landings, not before — the headline
will not move from 88 until an IMPORTANT lands.

---

## Findings, structured per brief

### NEW MINOR-latent — `Subscription::close` borrow_mut spans `waker.wake()`

```
[MINOR-latent] crates/plugin-db/src/broker.rs:359-369
  Why: Subscription::close holds `inner = self.0.borrow_mut()` at :360
       through w.wake() at :367. Same class as the previously-tracked
       Subscription::push hazard at :319-321 / :325-327. Compio's
       Waker::wake enqueues the task to the run queue without
       synchronously polling, so re-entry is impossible TODAY.
       Latent if a future Waker shim invokes the woken task
       synchronously and that task re-enters Subscription state on
       the same thread.
  Fix: drop `inner` before w.wake() (same shape as the push fix):
         let waker = inner.waker.take();
         drop(inner);            // release the borrow_mut
         if let Some(w) = waker { w.wake(); }
       Land together with the push fix at :319-321 + :325-327 — same
       class, same semantics, same one-liner each site.
  Repro: not exploitable under current compio scheduler. Would
       become exploitable if a future Waker variant invokes
       synchronously and the woken task re-enters this Subscription's
       RefCell on the same thread.
  Verification: broker.rs:357-369 (close body); broker.rs:306-328
       (push body — sibling sites); broker.rs:480-529 (publish — not
       a borrow holder across wake). r6/r7/r8/r9 enumerated push but
       missed close.
```

### CARRY-OVER IMPORTANT — `run_sql` cancellation drops `tx_conn` but leaves `pending_emits` queued

```
[IMPORTANT] crates/plugin-db/src/exec.rs:51-55 +
            crates/plugin-db/src/v8_classes/transaction.rs:240-246 +
            crates/plugin-db/src/v8_classes/transaction.rs:119-123
  Why: run_sql takes tx_conn out across an .await; cancellation
       between :51 (take) and :55 (put) drops the Client (PG observes
       torn sender → server-side ROLLBACK). pending_emits queued
       earlier in the same tx remain in the per-isolate slot.
       Transaction::end's client_opt=None early-out at :240-246
       sets settled=true and returns Ok WITHOUT calling
       drain_pending_emits_on_commit OR clear_pending_emits. The
       Drop finalizer at :119-122 sees settled=true and short-circuits
       BEFORE the :139 clear_pending_emits call. Residue persists
       until the next exec_begin's defensive clear at
       orchestrator/transaction.rs:172.
  Fix: in Transaction::end's client-already-cleared branch
       (transaction.rs:240-246) and Transaction::drop's
       settled-true early-out (transaction.rs:119-123), call
       crate::exec::clear_pending_emits() unconditionally before
       returning. One-liner each site. Matches exec_begin's
       defensive-clear discipline.
  Verification: exec.rs:43-73 (run_sql take/put pair);
       v8_classes/transaction.rs:119-141 (Drop early-out at :122);
       v8_classes/transaction.rs:240-246 (end early-out);
       orchestrator/transaction.rs:165-173 (defensive clear bounds
       the leak).
  Status: r6/r7/r8/r9 carry, unchanged.
```

### CARRY-OVER IMPORTANT — `exec_commit_batch` is_done path's `release_advisory_lock` cancellation window

```
[IMPORTANT] crates/plugin-db/src/migrations.rs:614-656
  Why: in the is_done terminal branch, finalise_backfill runs
       (warn-on-err per 51ced4a0), then release_advisory_lock
       awaits at :651, then drop(client) at :653, then
       clear_mig_lock at :654. If the future is cancelled between
       :651's await and :654's clear, mig_lock stays set with
       client=None (it was dropped at :653 if reached, or by the
       cancel unwind otherwise). Future migrationBegin calls see
       has_mig_lock=true at migrations.rs:228 and reject with
       `migration_already_active` until the operator runs reset().
  Note: the *other* release_advisory_lock site at migrations.rs:287-291
       (cancelled-row rejection in exec_begin) does NOT have this
       hazard, because set_mig_lock is only called later at
       migrations.rs:329 — the in-process slot is never set at the
       point of cancellation, so PG session-close auto-release is
       sufficient.
  Repro: migration on path A reaches is_done=true; JS Promise
       cancelled by V8 (e.g., isolate shutdown signal) between
       :651 await resolving and :654's slot clear. Server-side
       advisory lock release is observable (the .await returned
       OR the client drop on cancel closed the session), but the
       in-process mig_lock slot still says "Running".
  Fix: RAII guard family same as ConsumerRunningGuard
       (70921112+386f9bf5). New struct MigLockGuard owning the
       mig_lock slot; Drop calls clear_mig_lock unconditionally.
       Construct at the set_mig_lock site at :328-336; release via
       .into_persistent() (or analogous) when the caller
       intentionally keeps the lock across an await chain that
       doesn't own the cleanup. Drop fires on cancel/panic
       regardless of where in :651 → :654 the future unwound.
  Verification: migrations.rs:614-656 (is_done branch);
       migrations.rs:228-235 (the has_mig_lock check that rejects);
       migrations.rs:328-336 (set_mig_lock construction site);
       context.rs:362-372 (set_mig_lock / clear_mig_lock).
  Status: r6/r7/r8/r9 carry, unchanged.
```

### CARRY-OVER MINOR — `apply.rs` `update_audit_status` Err swallow

```
[MINOR] crates/plugin-db/src/orchestrator/register_model/apply.rs:163-186
  Why: success and failure branches both call update_audit_status
       via `let _ = backend.update_audit_status(...)`. If the audit
       UPDATE fails after the DDL succeeded, the audit row stays in
       Running with no operator-visible signal. Sibling of the
       finalise_backfill case 51ced4a0 closed in migrations.rs:635.
  Fix: same one-line shape as 51ced4a0:
         if let Err(e) = backend.update_audit_status(...).await {
             tracing::warn!(app_id=%app_id, audit_id=id, error=%e,
                 "update_audit_status failed; audit row may stay
                 Running until next reset()");
         }
       Apply twice (success branch at :163-171, failure branch at
       :178-185).
  Repro: a backend hiccup between DDL completion and audit UPDATE.
       The DDL is durable, the lock is released, but operators
       querying __zeroship_migrations.status see "Running" for
       what is a completed migration.
  Verification: apply.rs:160-188 (both branches use let _ today);
       migrations.rs:635-648 (the F1-pattern reference applied
       successfully in 51ced4a0).
  Status: r7/r8/r9 carry, unchanged.
```

### CARRY-OVER MINOR-latent — broker `push` borrow_mut spans `waker.wake()`

```
[MINOR-latent] crates/plugin-db/src/broker.rs:306-328
  Why: Subscription::push holds `inner = self.0.borrow_mut()`
       across w.wake() at TWO sites — overflow path :320 and
       normal-push path :326. Compio's Waker::wake enqueues the
       task to the run queue without synchronously polling, so
       re-entry is impossible TODAY. Latent if a future Waker
       shim invokes the task synchronously.
  Fix: drop `inner` before w.wake():
         let waker = inner.waker.take();
         drop(inner);          // release the borrow_mut
         if let Some(w) = waker { w.wake(); }
       Two sites: overflow path (:319-321) and normal push
       (:325-327). Land together with the new r10 close() finding
       at :366-368 — same class, same fix shape.
  Repro: not exploitable under current compio scheduler.
  Verification: broker.rs:306-328 (push body); broker.rs:480-529
       (publish — note: publish itself does NOT hold a broker
       borrow across wake; hazard is inside push). r6/r7/r8/r9
       unchanged disposition.
  Status: r6/r7/r8/r9 carry, unchanged.
```

### CARRY-OVER MINOR — CIC retry budget × pool depth (transient saturation)

```
[MINOR-transient] crates/plugin-db/src/backend/postgres.rs:385-600
  Why: MAX_RETRIES = 3 caps the retry budget; each retry holds a
       brief pool checkout. N concurrent register_model calls
       all hitting CIC for distinct indexes can transiently
       saturate the pool (default 16). Not unbounded growth;
       transient.
  Fix: not required — re-classified r7 as transient saturation,
       not unbounded. Could explore a CIC-coalesce or queue but
       not blocking.
  Status: r7/r8/r9 re-class, unchanged.
```

### COSMETIC — `OrchestratorLockGuard::release()` returns `Result<_, DbError>` but is infallible

```
[COSMETIC] crates/plugin-db/src/orchestrator/lock_guard.rs:144-183
  Why: re-walked at HEAD — the unlock-SQL Err path now logs via
       tracing::warn! at :172-179 (post-[I44]) and falls through
       to `self.released = true; Ok(self.client.take())`. All
       three call sites still use `let _ = guard.release().await`.
  Fix: either
       (a) tighten signature to `async fn release(mut self) ->
           Option<PooledClient<'p>>` (drop the Result wrapper); or
       (b) keep Result for future-proofing but document the
           current infallibility.
  Repro: N/A (no observable behaviour to break).
  Verification: lock_guard.rs:144-183 (function body has no early
       Err return — the only error path is the warn-and-fall-through
       at :168-179).
  Status: r6/r7/r8/r9 carry, unchanged.
```

---

## Invariants that held up under r10 audit

- **`386f9bf5` latent bug fix.** `won.then(|| Self { app_id })` lazy
  construction continues to be correct.
- **`e5315083` API gating.** `mark_consumer_running` remains
  `#[cfg(any(test, feature = "test-helpers"))]`.
- **r7 NEW MINOR-R7-1 stays closed.** Atomic try-claim closes the
  dispatch-return-vs-first-poll race window by construction.
- **All five ConsumerRunningGuard lifecycle scenarios.** (A)
  future-dropped-pre-poll, (B) race-between-spawns, (C)
  panic-in-loop, (D) graceful-CopyDone, (E) fatal-Err. Each leaves
  the slot in a defined state.
- **SuppressGuard ⊂ ConsumerRunningGuard lifetime nesting.**
  Unchanged. LIFO unwind on panic.
- **OrchestratorLockGuard 4-layer hardening.** All four design-pass
  commits still cross-walked against the source. release() Result
  remains cosmetic.
- **PENDING_EMITS visibility ordering.** drain fires after COMMIT
  `.await` resolves AND after the client is dropped. Defensive
  clear at every exec_begin bounds residue from the run_sql
  cancellation IMPORTANT.
- **TX_CONN ownership.** Same six mutation sites as r6/r7/r8/r9.
  Token discipline via `debug_assert!` unchanged.
- **Compio task scheduling assumption** (single-thread per isolate)
  unchanged. Every plugin-db spawn captures `!Send` data.
- **Subscription::next .await discipline** (NEW for r10
  cross-check). The `next()` method at
  `v8_classes/subscription.rs:96-127` correctly snapshots
  `sub_opt: Option<BrokerSubscription>` via
  `self.inner.borrow().as_ref().cloned()` *before* the `poll_fn`
  await. No RefCell borrow is held across the await. The post-await
  `self.inner.borrow_mut().take()` at `:123` runs sync. **Sound.**

---

## Recent-commit concurrency-neutrality matrix

| Commit | File(s) | Change kind | Sync surface? |
| --- | --- | --- | --- |
| `7d0bc4c5` | `exec.rs:64,317` | Rename `not_configured` → `lazy_init_failed` | No (audited in r9) |
| `389749ca` | `v8_classes/migration.rs:269` + `v8_classes/migrations.rs:180` | Rename `not_configured` → `backend_not_initialized` | No (`&'static str` constant rename) |
| `7bd2187e` | `benches/bench_query_build.rs` + `Cargo.toml` | Bench scaffold | No (outside `src/`) |
| `757026e3` | `audit.rs`, `error.rs`, `orchestrator/mod.rs`, `query.rs` | `///` doc fixes | No (comments) |
| `bed655c1` | `replication.rs` (test docstring) | `///` doc fixes | No (comments) |
| `a6dca645` | `docs/reviews/`, `docs/backlog/` | Backlog audit | No (no src/) |

**Verdict:** zero synchronisation-path changes in the r9 → r10 window.

---

## Score: **88 / 100** (no change vs r9: 88)

Why no movement on the floor:

- **One new MINOR-latent finding** (broker `close()` borrow across
  wake) — same class as the existing push MINOR-latent; not gating.
- **Zero carries closed.** The three carry-over IMPORTANTs/MINORs
  persist verbatim. No commit in this window touched their files.
- **Recent commits are docs/visibility/rename/bench-scaffold only.**
  None could have closed a concurrency finding because none touched
  synchronisation paths.

Why the score isn't lower:

- **No regressions.** The 6 commits in window are concurrency-
  neutral. Re-walked each diff against r9's audited form; the
  body of every synchronisation-sensitive function is byte-
  identical to r9's reading.
- **All r9 invariants still hold.** ConsumerRunningGuard atomic
  try-claim, OrchestratorLockGuard 4-layer hardening,
  PENDING_EMITS visibility ordering, TX_CONN ownership, compio
  single-thread assumption — all unchanged.

| Round | Score | CRITICALs | IMPORTANTs | New findings |
| ----- | ----- | --------- | ---------- | ------------ |
| r2    | 82    | 0         | 4          | several      |
| r3    | 73    | 1 (boot)  | 4          | several      |
| r4    | 80    | 0         | 4          | 1 MINOR      |
| r5    | 80    | 0         | 5          | 1 MINOR      |
| r6    | 84    | 0         | 3 (2 closed) | 0          |
| r7    | 86    | 0         | 2 (1 closed) | 1 MINOR    |
| r8    | 88    | 0         | 2 (carry) | 0 (1 r7-MINOR closed) |
| r9    | 88    | 0         | 2 (carry) | 0 (no commits in window touched sync paths) |
| r10   | **88** | 0       | **2** (carry) | **1 MINOR-latent** (broker close site that r6-r9 all missed) |

---

## Comparison vs r9 — explicit

| Aspect | r9 | r10 | Δ |
| ------ | -- | --- | - |
| Score | 88 | **88** | 0 |
| IMPORTANTs | 2 carry | 2 carry | 0 |
| MINOR-latent (broker waker) | 2 sites (push ×2) | **3 sites (push ×2 + close ×1)** | **+1** |
| Carries closed | 0 | 0 | 0 |
| Asymptote w/o landings | 89 | **88.5-89** | ≈0 |
| Asymptote w/ IMPORTANTs landed | 92-93 | 92-93 | 0 |
| Asymptote w/ all carry-over landed | 95 | **95.5** | +0.5 |

**Headline movement: zero.** The new finding nudges the
post-landing ceiling by half a point because it's a separable
landing slot, but does not affect the gating constraint (two
IMPORTANTs unlandedness). It IS evidence that the lens is not
perfectly exhausted — even after 4 rounds of fresh re-audits, a
same-class site was missed. But the marginal yield (one MINOR-latent
in 4 rounds) confirms r9's diminishing-returns call.

---

## Plateau call — refreshed

**r9 said:** "without landings, asymptote is 89; with both IMPORTANTs
landed, 92-93."

**r10 confirms:** the headline plateau call is still correct. The
new finding does not move the headline; it adds a third site to an
existing MINOR-latent class. Marginal value of further re-audits
without landings continues to be very low — approximately one
MINOR-latent finding per 4 rounds, all bounded by current compio
Waker semantics.

**Concrete next-cycle recommendation (unchanged from r9):**

1. Land the run_sql cancel `clear_pending_emits` two-liner. +2.
2. Land the `MigLockGuard` RAII abstraction. +2.
3. Land the `update_audit_status` warn-instead-of-swallow two-liner. +1.
4. Land the broker waker borrow fix at ALL THREE sites (push :320,
   push :326, close :367) as one commit. +0.5.

These four landings put the headline at 93.5 in a single cycle.
The release() Result tightening is cosmetic (+0.5 → 94 ceiling
without new code).

**For the cycle 10:17+ plateau context:** the lens has effectively
converged. Further concurrency rounds without landings will return
the same dispositions plus possibly one more same-class MINOR-latent
finding. The next scheduled review should be AFTER the two
IMPORTANTs land; running r11 against unchanged sync paths is work
that returns ~0.25 findings/round, none of them gating.

---

## Closing summary

R10 finds **one new MINOR-latent**: `Subscription::close` at
`broker.rs:359-369` holds `inner = self.0.borrow_mut()` across
`w.wake()` at `:367` — same class as the previously-tracked
`Subscription::push` MINOR-latent at `:319-321` / `:325-327`, but a
*third site* that rounds r6, r7, r8, and r9 all missed. Safe under
current compio Waker semantics (enqueue, not synchronous poll);
becomes exploitable only if a future Waker shim polls synchronously
AND the woken task re-enters the same Subscription state. Fix is the
same one-liner per site (extract waker, drop inner, then wake).

The three carry-over findings (run_sql cancel `pending_emits` residue;
`exec_commit_batch` is_done window; `apply.rs update_audit_status`
swallow) persist **verbatim** — re-walked at HEAD line-for-line, every
synchronisation-sensitive function body is byte-identical to r9's
reading.

The six commits between r9 and r10 (`7d0bc4c5` audited in r9,
`389749ca`, `7bd2187e`, `757026e3`, `bed655c1`, `a6dca645`) are
entirely error-rail evolution, documentation accuracy, bench
scaffolding, and review/backlog files — four categories of work that
by construction cannot regress concurrency. Re-walked every diff
against the r9-audited form; every synchronisation-sensitive function
body is byte-identical.

**Score: 88 / 100 (r9: 88 / 100, Δ = 0).** Zero headline movement
reflects zero work in the concurrency lens during this cycle. The
score floor (88) is robust — recent commits did not regress. The
new broker-close finding adds +0.5 to the post-all-landings ceiling
(95 → 95.5) but does not affect the gating constraint. **The lens has
converged for the headline; the next concurrency cycle should be
scheduled after the two IMPORTANTs land, not before.**
