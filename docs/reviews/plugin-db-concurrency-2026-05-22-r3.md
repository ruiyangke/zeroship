# plugin-db concurrency / lifecycle review — 2026-05-22 r3

**Commit:** 5be3c1a1 (HEAD)
**Lens:** concurrency + lifecycle (per task brief)
**Scope:** advisory-lock release, TX_CONN ownership, MIG_LOCK ↔ audit
coordination, WAL consumer task lifecycle, broker fanout re-entry,
pending-emits flush timing, cross-isolate thread-local discipline, pool
exhaustion under retry.

Prior round: `docs/reviews/plugin-db-concurrency-2026-05-22-r2.md` (82 / 100).

---

## Headline

The two recent advisory-lock leak fixes (`apply.rs` pass-1 wrap and
`run_pipeline` plan/validate unlock) close two of the three legs of the
register_model cross-app stall, but **the symmetrical leak inside
`bootstrap()` itself is still open**: every `?` propagation between
`acquire_advisory_lock` and the function's `Ok((ctx, lock_client))`
return drops a still-held PooledClient back into the pool. Below this
new CRITICAL finding, the R2 backlog is largely intact (R2's `mig_lock`
on dry-run COMMIT-fail; the `running_consumers` task-cancellation gap),
plus one new IMPORTANT (the symmetrical case in `exec_commit_batch`
when the post-COMMIT audit progress write fails) and a refined account
of MIG_LOCK ↔ audit_row coordination.

---

## Findings

### CRITICAL — `bootstrap()` propagates `?` past the acquired lock without unlocking

**File:** `crates/plugin-db/src/orchestrator/register_model/bootstrap.rs:103-145`

`bootstrap()` acquires the advisory lock on `lock_client` at line 107–119
and then runs four fallible steps **on the pool** (NOT on
`lock_client`) before returning:

```rust
backend.acquire_advisory_lock(&lock_client, &key, LOCK_TAG).await?;  // ✓ lock now held on lock_client
backend.ensure_app_schema(app_id).await?;        // pool.exec — if Err, ? unwinds
backend.ensure_audit_table(app_id).await?;       // pool.exec — if Err, ? unwinds
let schema_version = backend.next_schema_version(app_id).await?;  // pool.exec — if Err, ? unwinds
let mut declared_indexes =
    query::build_create_indexes(app_id, collection, schema).map_err(DbError::from)?;  // CPU — if Err, ? unwinds
let named_indexes =
    query::build_named_indexes(app_id, collection, indexes).map_err(DbError::from)?;  // CPU — if Err, ? unwinds
```

`backend.ensure_app_schema`, `ensure_audit_table`, `next_schema_version`
all execute through `self.pool` (verified at
`backend/postgres.rs:164-197`), NOT through `lock_client`. The lock is
on `lock_client`'s **session**, which is a `PooledClient` whose Drop
**returns the connection to the pool with the session-scoped advisory
lock still held**.

**Why:** Identical mechanism to the bug the b4e533e2 patch fixed in
`run_pipeline()`. The fix in b4e533e2 + 37a0ef76 covers plan/validate
and Pass-1; the symmetrical hole upstream of plan — inside bootstrap
itself — was not patched. The b4e533e2 commit message says the fix
"closes the symmetrical gap upstream of apply" but it only covers the
gap *between bootstrap return and apply* — bootstrap's own internals
were skipped.

**Repro:**
1. `bootstrap` calls `backend.acquire_advisory_lock(...)` on
   `lock_client` → SUCCESS, session-scoped lock held on the pooled
   connection.
2. `ensure_app_schema` runs against the pool. The pool's CREATE SCHEMA
   query happens to fail (disk full, PG restart mid-deploy, network
   blip, app_id with a colon, etc.) → `Err(DbError::…)`.
3. `?` propagates. The bootstrap function's stack unwinds.
4. `lock_client: PooledClient<'p>` is dropped via Rust drop semantics
   → goes back to the pool with the session-scoped advisory lock still
   held.
5. Next `registerModel` on the same app_id calls
   `backend.acquire_advisory_lock(...)` on a different `PooledClient`
   borrowed from the pool. The acquire query may *itself* hit the
   poisoned connection (pool's LRU/FIFO chooses one), but even if it
   doesn't, the per-app advisory lock is now held in another session
   that the pool will hand out to anything. Subsequent
   `pg_advisory_lock(hashtext('zs_reg:<app>'), …)` calls block forever
   on whichever pooled connection happens to be the one holding the
   lock — the exact failure mode the b4e533e2 commit message described
   for the apply-Pass-1 leak, just one stage earlier in the pipeline.

**Fix:** Wrap the post-acquire portion of `bootstrap` in an async block,
capture the Result, and on Err run the same `pg_advisory_unlock` SQL
that `apply.rs` and `run_pipeline()` emit before dropping `lock_client`.
Equivalently: thread the unlock through a `Drop` guard on
`(PooledClient, key, LOCK_TAG)` — a small `LockGuard` struct whose
`Drop` schedules an unlock would close every variant of this leak in
one place.

**Verification:**
- `bootstrap.rs:103` (acquire on `lock_client`).
- `bootstrap.rs:107-119` (acquire SQL).
- `bootstrap.rs:126-144` (four `?` exits after acquire).
- `backend/postgres.rs:164-172` (`ensure_app_schema` routes through
  `self.pool`).
- `backend/postgres.rs:188-197` (`ensure_audit_table`,
  `next_schema_version` route through `self.pool`).
- `run_pipeline()` lines 213-227 release the lock on plan/validate
  failure — *outside* `bootstrap`. So if bootstrap fails, that handler
  never runs.

### IMPORTANT — `exec_commit_batch` returns the lock client after a successful COMMIT when the audit-progress write fails

**File:** `crates/plugin-db/src/migrations.rs:553-577`

The batch loop runs COMMIT (or ROLLBACK for dry-run) at line 554-558,
then writes audit progress at line 562-577. If COMMIT/ROLLBACK
succeeds but `update_backfill_progress` fails:

```rust
if let Err(e) = backend.update_backfill_progress(&client, ...).await {
    return_lock_client(client);  // line 574
    return Err(coded_db("audit row update", e));
}
```

The client is parked back in `mig_lock.client` with the per-app
migration advisory lock still held (the migration uses
`acquire_dedicated_client`, so the lock survives the put-back —
session ends only when the Client is *dropped*, not when it's parked).
The mig_lock state is still `Some` → next `commitBatch` or
`fetchBatch` call sees `has_mig_lock() == true`, calls
`take_lock_client()`, gets the client, but the audit row is in an
intermediate state (last successful cursor reflects the in-server
COMMIT, in-process state shows the cursor before COMMIT). The SDK's
retry loop may double-process a window of rows.

R2 flagged the dry-run COMMIT-fail and real-run COMMIT-fail cases;
this is the *post*-COMMIT audit-write-fail case, which is strictly
worse because the underlying tx already landed.

**Fix:** When the audit-progress write fails after a successful COMMIT,
the migration must be marked failed (audit row transitioned to
`failed`) AND the lock released. The cleanest path is to take a final
"terminal_failed" path symmetric to the `is_done` block at lines
581-606: write the failed audit row using the client, release the
advisory lock, drop the client.

**Repro:**
1. App calls `commitBatch(updates=[...], isDone=false)`.
2. BEGIN, audit-row lock, all UPDATEs succeed.
3. COMMIT succeeds — the data is durable.
4. Network blip / PG kills the connection between COMMIT response and
   `update_backfill_progress`'s next round-trip.
5. `update_backfill_progress` returns Err. Function returns Err.
6. Migration is stuck "running" forever; the SDK retries, hits the
   `take_lock_client` slot (still occupied), eventually times out.
   Operator must call `cancel()` to clear, but `cancel()` reads the
   audit row, which still shows the old cursor — so a fresh `start()`
   re-processes the COMMITed batch.

**Verification:** `migrations.rs:553-577`; the contrast at lines
581-606 shows what the terminal path *does* do (release lock + clear
mig_lock).

### IMPORTANT — `running_consumers` slot diverges from a live task on rapid teardown (carried from R2)

**File:** `crates/plugin-db/src/replication_ops.rs:250-257`

R2 finding still open. `mark_consumer_running` runs at line 250
*before* the `compio::runtime::spawn`, and `unmark_consumer_running`
runs *inside* the spawned future after `run_supervised` returns. If
the spawned future is dropped before it starts polling (e.g. the
compio runtime is shutting down between the spawn call and the first
poll, or the isolate is being evicted by the worker's LRU), the
unmark never runs. The slot stays "running" but no task is live, so
a subsequent `startReplicationConsumer()` short-circuits with
`alreadyRunning: true` and the app has no consumer.

R2's recommended fix (RAII Drop guard for the `app_for_task` capture)
still applies. The recent fix at `967a7362` (early-return on no
subscribers in `emit_for_tuple`) does NOT change this race — it's a
perf change inside the consumer's decode loop, orthogonal to the
supervisor's slot bookkeeping.

**Verification:** `replication_ops.rs:250` is `mark`; line 251 is
`spawn`; line 255 is `unmark` *inside* the spawned future. The Drop
guard pattern at `wal_consumer.rs:139-159` (SuppressGuard) is the
exact shape that should wrap `app_for_task` here.

### IMPORTANT — `exec_commit_batch` leaves `mig_lock` slot occupied on dry-run COMMIT failure (carried from R2)

**File:** `crates/plugin-db/src/migrations.rs:553-558`

R2 finding still open. If the dry-run ROLLBACK (`final_sql = "ROLLBACK"`
under `dry_run=true`) fails, the function returns at line 557 after
`return_lock_client(client)` — the client is parked back with the
advisory lock still held on its session. `mig_lock.client = Some(...)`,
`mig_lock = Some(...)`, so `has_mig_lock()` still returns true. The
isolate is stuck in "migration active" until the wrapper is GCed and
the finalizer fires `exec_cancel`. Same shape as the real-run COMMIT
failure (also at line 555-558).

**Verification:** `migrations.rs:555-558` is the COMMIT/ROLLBACK error
branch; no `clear_mig_lock` on that path.

### IMPORTANT — `run_sql` cancellation drops the tx client; `pending_emits` is not cleared

**File:** `crates/plugin-db/src/exec.rs:43-57`

`run_sql` does `take_tx_client → await → put_tx_client`. If the
spawned op future is *dropped* (not awaited to completion) between the
take and the put — e.g. the V8 callback's spawned future is
cancelled because the isolate is being evicted, or the request's
state is torn down — the `client: Client` local variable is dropped,
the per-tx connection closes, Postgres rolls back. But two pieces of
state remain:

1. `tx_token` was set by `begin_transaction_dispatch` (line 81 of
   `orchestrator/transaction.rs`) and was NOT cleared by the
   cancelled future. `Transaction::end` would later observe
   `tx_token == this.token` and `take_tx_client` returns None, falling
   into the line-237 "tx_conn already cleared" branch which returns
   `Ok(())` *without calling `clear_pending_emits()`*.
2. `pending_emits` for the cancelled tx remains in the queue.

If the next code path is `exec_begin` / `exec_auto_begin`, the
`clear_pending_emits()` call at the start of those functions resets
the queue. But if no new transaction starts before the next pool
mutation that emits — there isn't one in the autocommit path
(`queue_or_emit` checks `has_tx`, which is false because `tx_conn` is
None) — the stale queue accumulates events through every isolate
lifetime until a fresh BEGIN clears it. Memory-only leak; not a
correctness bug because the events are only fired by
`drain_pending_emits_on_commit`, which Transaction::end's early-out
branch doesn't call.

**Why:** Transaction::end at lines 237-244 returns Ok early when the
client is already gone, without distinguishing "another path drained
me" (the desired no-op) from "the previous operation got cancelled
mid-await and the queue is now garbage". The R2 review flagged a
related concern about `drain_pending_emits_on_commit`'s suppression
check; this is a different shape.

**Fix:** In Transaction::end's `client_opt is None` branch, also call
`crate::exec::clear_pending_emits()` so the stale queue is dropped at
the same time `tx_token` and `settled` are reset. Symmetric with the
Drop impl at lines 117-138 which DOES call `clear_pending_emits` (line
137).

**Repro:** Hard to weaponize today — the compio runtime keeps spawned
ops alive until they complete, so the cancellation path requires
isolate eviction or a panic + catch_unwind. The pre-condition is
narrow but the fix is one line.

**Verification:** `v8_classes/transaction.rs:237-244` (early-out);
`v8_classes/transaction.rs:267-268` (Drop's explicit
`clear_pending_emits`); `exec.rs:42-57` (`run_sql` take/put).

### MINOR — `update_audit_status` failure leaves the audit row stuck in `Running`

**File:** `crates/plugin-db/src/orchestrator/register_model/apply.rs:139-167`

`run_op` writes a `Running` audit row up-front, executes the DDL,
then best-effort calls `update_audit_status` with the result:

```rust
let _ = backend.update_audit_status(&app_id, id,
    crate::audit::TerminalStatus::Applied, None).await;  // line 142-149
```

The `let _ = …` discards any Err. If the DDL succeeded but the
audit-update fails (a transient PG hiccup between the DDL response and
the audit-table UPDATE), the row stays at `status = 'running'` forever.

This isn't directly weaponizable: `next_schema_version` filters
`WHERE status = 'applied'`, so a stuck-running row doesn't poison
subsequent schema-version reads. But operator dashboards reading the
audit table see a forever-running row that actually completed — a
silent correctness gap for the audit invariant.

The audit row's `Running → Applied | Failed` transition isn't
guaranteed; the comments at `audit.rs:135-150` describe the state
machine as if it were, but the apply path discards update errors.

**Fix:** If `update_audit_status` returns Err after a successful DDL,
queue a retry (e.g. via a `tracing::warn!` + a maintenance-cron
reconciler that walks `__zeroship_migrations` looking for rows whose
`updated_at < now() - interval '10 minutes'` and whose deploy_id is
older than the current generation). Alternatively, escalate the audit
failure into the function Result so the operator sees the deploy as
"failed" even though the DDL itself succeeded — defensible because the
audit row's state IS part of the deploy's committed state.

**Verification:** `apply.rs:142-167` (best-effort error swallow at both
Ok and Err arms); `audit.rs:338-368` (terminal-status SQL — the
`WHERE status IN ('running','pending')` predicate means a second
attempt at the same id would succeed if retried).

### MINOR — broker `publish` is not re-entry safe if a Waker callback synchronously re-publishes

**File:** `crates/plugin-db/src/broker.rs:433-470` + `547`

`broker::publish()` does `BROKER.with(|b| b.borrow_mut().publish(event))`.
The iteration calls `s.push(...)` → `waker.wake()`. Standard async
runtimes treat `Waker::wake` as a deferred enqueue (no synchronous
callback), so this is safe today. But the broker's contract doesn't
require a deferred waker — a future Waker impl that synchronously
calls back into user code, or a Waker passed across an FFI boundary
that synchronously fires a closure, would cause
`RefCell::borrow_mut` to panic on the second `BROKER.with`.

The comment at `broker.rs:454-456` explicitly notes the loop holds
`&` on `subs` and assumes no recursive `publish`. The assumption is
fragile — the recursion would not be from inside the broker code, it
would be from `Waker::wake` triggering arbitrary user code. Today's
runtime keeps this safe; it's documented only in a comment.

**Fix:** Take a snapshot of `Subscription` clones out of the bucket
before the loop, drop the borrow_mut, then push. This costs one
small Vec allocation per publish but makes the borrow window
arbitrarily small. Already acceptable cost (broker is per-isolate
single-threaded; publish rate is bounded).

**Verification:** `broker.rs:457-462` iterates `subs.iter()` while
`&mut self` is live. `broker.rs:319-327` and 365-367 fire wakers
under `inner.borrow_mut()`. `broker.rs:551-553` re-enters BROKER on
every publish call.

### MINOR — `dispatch_*` retries against `create_index_with_recovery_audited` are unbounded × pool depth

**File:** `crates/plugin-db/src/backend/postgres.rs:387-619`

The CIC retry loop runs up to `MAX_RETRIES = 3` attempts. Each attempt
takes a pooled client implicitly via `pool.query_text_params` (line
457), then a second client for the validity check (line 468), then
optionally a third for `drop_idx_sql` (line 489), plus audit writes
(lines 479-487, 521-530, 560-568).

For a 10-pool-depth `Pool` with a single CIC running, the loop
consumes ~6 short-lived borrows per attempt × 4 attempts = ~24
sequential acquisitions. They're sequential (each await completes
before the next take), so no exhaustion under single-tenant load.

The risk arises under N concurrent register_model calls × N apps
each running their own CIC retry: 6N borrows interleave with the
pool's other consumers. For Pool::DEFAULT_MAX_SIZE = 16 (default
compio-postgres) this is fine; for a smaller configured size or
multi-app concurrent deploys, it could spike to "all clients busy on
CIC audit writes" momentarily.

Not a leak, not a deadlock, but worth noting: the retry path's
audit-write fan-out is unbounded × pool size.

**Verification:** `backend/postgres.rs:457, 468, 489, 521-530, 560-568,
571, 597-598` — each is an implicit pool acquire.

---

## Invariants that held up under r3 audit

These are R2's "invariants worth documenting" plus what r3 confirmed
remains intact:

- **TX_TOKEN ↔ TX_CONN coherence**: `set_tx_token` still asserts
  non-zero ⇒ `tx_conn = Some` (`context.rs:293-299`). Every settle
  path drains the client before clearing the token. R3 audit confirms
  this invariant in `auto_tx.rs:219-223`, `transaction.rs:237-251`,
  `transaction.rs:127-138` (Drop).
- **`pending_emits = None` outside a transaction**: Both
  `transaction.rs::exec_begin` (line 171) and `auto_tx.rs::exec_auto_begin`
  (line 199) call `clear_pending_emits` before the BEGIN returns.
  Confirmed.
- **Auto-tx ownership token**: R2 noted the auto-tx token is the fixed
  literal `1` (`auto_tx.rs:200`); this is still the case. The asymmetry
  with the user-tx `tx_token` is not a concurrency bug — the auto-tx is
  a defense-in-depth wrapper around the B3 capability gate — but
  staged-rollout JS that calls `__zsEndAutoTx(1, true)` from a
  different stack frame than the matching `__zsBeginAutoTx` would
  commit something it doesn't own. Untouched since R2.
- **WAL consumer same-thread broker access**: `emit_for_tuple` short-
  circuit at `wal_consumer.rs:548` reads `BROKER` from the same compio
  thread as the consumer's decode loop and as any subscribe call.
  The recent `967a7362` change is race-free in the single-thread model.
- **Cross-isolate isolation**: compio's task scheduler does NOT migrate
  tasks across threads (Send is not required by `compio::runtime::spawn`).
  All `thread_local!`s (`BROKER`, `ISOLATE_CTX`, `SUPPRESSED_APPS`,
  `MODEL_REGISTRY`, `read_set.rs::READ_SET_BUILDER`) are accessed only
  from compio tasks within one runtime. No cross-thread panics
  possible from the runtime contract alone.
- **Replication schema casing**: `a00c41fd` fix correctly uses
  `quote_ident(app_id)` for the publication's schema reference, and a
  regression test pins the behaviour. Audit confirmed at
  `replication.rs:150-169`.
- **RPC dispatch stream sync return**: `cac3e542` only touches
  bootstrap dispatcher + `runtime/src/core/init.rs`; no concurrency
  surface in plugin-db changed. Out of scope for this review's lens
  but noted as confirmed-clean.

---

## Score: 73 / 100  (down from 82)

Why the drop:

- **One new CRITICAL** (bootstrap's symmetrical lock leak): the kind
  of bug the b4e533e2 commit was meant to eliminate, just one frame
  earlier in the pipeline. The recent fixes addressed the surface area
  that the integration suite happened to exercise; the static gap
  remains, and any pool-shared connection that survives the bug
  becomes a tarpit for the next deploy.
- **One new IMPORTANT** (`exec_commit_batch` post-COMMIT
  audit-progress-fail leaves mig_lock occupied): R2 caught two of the
  three commit-batch hole shapes; the third is the post-COMMIT one and
  is strictly worse than the pre-COMMIT cases because the data has
  already landed.
- **One refined IMPORTANT** (`run_sql` cancellation pending-emits
  leak): R2 didn't catch this; the cancellation window is narrow but
  the fix (one `clear_pending_emits` call in Transaction::end's empty-
  client branch) is trivial.

R2 findings still open (`running_consumers` rapid-teardown,
`exec_commit_batch` dry-run COMMIT-fail mig_lock leak,
`drain_pending_emits_on_commit` suppression-check conservatism,
`Migration::Drop` undocumented lock-release sequence) — none addressed
since R2.

Two recent fixes (b4e533e2 + 37a0ef76) are sound at the surface area
they cover; the audit-row state-machine documentation
`apply.rs::run_op` remains *aspirational* (the `Running → Applied |
Failed` transition isn't actually guaranteed because
`update_audit_status` errors are discarded). The replication schema
casing fix (`a00c41fd`) is correct and well-tested.

The headline change since R2 is that two of the four legs of the
register_model cross-app stall (Pass-1 and plan/validate) are closed;
the third (bootstrap-internal) remains, and the comment chain in
`mod.rs:174-200` and `apply.rs:172-200` reads as if the surface had
been thoroughly walked — which raises confidence-in-the-fix above its
actual coverage. The bootstrap leak is real, weaponizable on the first
pool connection that observes a CREATE SCHEMA / audit-table failure,
and the fix is one async-block wrap away.
