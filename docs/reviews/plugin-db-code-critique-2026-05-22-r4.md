# plugin-db code critique — 2026-05-22 R4

**Score trajectory: 78 (R1) → 85 (R2) → 87 (R3) → 88 (R4)**

Scope: `crates/plugin-db/` at HEAD (post-`cbd12944`).
Lens: Rust correctness and idioms only. Architecture / security / perf live in sibling reviews.

Re-audited fresh against the brief's eight dimensions. The
`OrchestratorLockGuard` (`cbd12944`), auto_tx typed-error rail
(`8ff1b2de`), broker two-level HashMap (`0e58c4e8` / `b32ba383`),
typed-RETURNING for replication (`c83d6a8c`), and the
`exec_mutation_with_emit` skip gate (`49b0b98e`) are all real progress.
None introduce regressions on the R3 score. The ceiling on this round
moves up by one net point: two MAJOR-class items unmasked by the new
code partially offset improvements (lock_guard's documented-but-real
drop leak; `mint_subscription` broker-entry leak on V8 alloc failure).

Findings below are tagged `[CRITICAL] / [MAJOR] / [MINOR] / [INFO]` per
the brief, with `file:line` evidence and verification commands.

---

## Verified recent commits

### `cbd12944` — `OrchestratorLockGuard` RAII

**Verified.** The guard centralises the three previously-inline
`pg_advisory_unlock` sites (`bootstrap.rs:137`, `apply.rs:226`,
`mod.rs:217`) behind a typed `release().await` / `into_held()` API.
Doc block on `lock_guard.rs:1-49` is excellent — it names the three
pre-refactor commits, the four call sites, and the three exit modes
(release / into_held / catastrophic Drop). Test coverage covers the
`released` flag transitions and idempotent `release()` (`lock_guard.rs:202-274`).

Two observations:

1. The `into_held()` exit mode (`lock_guard.rs:144`) is `#[allow(dead_code)]`
   — no caller uses it. The doc block (`lock_guard.rs:132-143`) is
   defensive about it ("kept codified at the guard boundary rather than
   re-discovered as another open-coded unlock"), which I buy. Note this
   is unenforced: a future contributor can add a caller that violates
   the "downstream stage will release later" contract and the guard
   gives no help — there's no token / receipt mechanism. M-NEW-3 below.

2. The `Drop` impl (`lock_guard.rs:159-182`) downplays the consequence
   of the fallback path. Comment says "until then any caller blocked on
   `pg_advisory_lock(zs_reg:<app>, register_model)` will stall." That's
   accurate but understates: the still-locked client returns to the pool
   via `PooledClient::Drop` and the pool can hand the same connection
   out to a future caller — who now owns a session lock they didn't
   acquire and can't release with a matching key/tag pair (they don't
   have them). M-NEW-1 below.

### `8ff1b2de` — auto_tx error rail → `OpResult::JsValue + RejectError`

**Verified.** Both `auto_begin_transaction` (`auto_tx.rs:60-73`) and
`auto_end_transaction` (`auto_tx.rs:97-108`) now route through
`OpResult::JsValue` with `ResolveValue::RejectError(e.to_op_error())`,
so a 40001 / 55P03 / class-08 error at the COMMIT/ROLLBACK boundary
reaches JS with `.code` intact. The two helpers `begin_to_resolve_value`
(`auto_tx.rs:120-125`) and `end_to_resolve_value` (`auto_tx.rs:134-139`)
are clean: small, total, pure conversion functions, easy to test
without a V8 scope. The two preserve-code unit tests
(`auto_tx.rs:326-345`, `auto_tx.rs:347-361`) pin the canonical
`transient` / `lock_not_available` codes the SDK branches on.

The two helpers' type signatures are well-bounded:
`fn(Result<u32, DbError>) -> ResolveValue` and
`fn(Result<(), DbError>) -> ResolveValue` — no lifetime gymnastics, no
generics, no impl-trait surface; trivially inlineable. Brief item 8
(type ascription) closes cleanly.

### `0e58c4e8` / `b32ba383` — broker two-level HashMap

**Verified.** The bucket layout (`broker.rs:402-409`) replaces the
prior `HashMap<(String, String), Vec<Subscription>>` with
`HashMap<String, HashMap<String, Vec<Subscription>>>`. The hot-path
lookup (`broker.rs:485-490`, `broker.rs:460-468`) now goes via `&str`
borrows on both keys — zero allocations on `publish` / `has_subscribers`.
Owned String allocation happens only on `subscribe` (`broker.rs:437-440`),
which the doc block correctly labels as the cold path.

Cleanup is also correct: empty inner bucket + empty outer bucket both
get dropped (`broker.rs:524-528`), so apps that churn through
ephemeral collections don't leak inner HashMaps. The
`publish_drops_per_app_map_when_last_collection_empties` whitebox test
(`broker.rs:1299-1310`) pins this.

### `c83d6a8c` — replication empty-RETURNING typed

**Verified.** `replication.rs:240-249` swaps the prior
`.first().map(...).unwrap_or_default()` for an explicit
`.ok_or_else(|| DbError::Internal { ... }.into_string())` — the empty-
RETURNING sentinel that mirrored the audit-id=0 bug now surfaces as a
loud error. The `.into_string()` at the tail is the legacy String-rail
adaptor; the underlying typed error is correct. Once `replication.rs`
migrates to `Result<_, DbError>` (I2 in R3), this naturally becomes
`.ok_or(DbError::Internal { ... })?`.

### `49b0b98e` — exec_mutation_with_emit skip gate

**Verified.** `exec.rs:189-250` extracts the gating logic into
`emit_for_rows` and short-circuits on two conditions:
`wal_consumer::is_app_suppressed(app_id)` and
`!broker::has_subscribers(app_id, collection)`. Both are constant-time
thread-local probes. The 5 unit tests (`exec.rs:405-...`) drive each
branch explicitly. Comments at `exec.rs:160-188` document the
conservative-true contract — a subscribe between the probe and the
next mutation sees the next mutation's event, no race.

One subtle issue, M-NEW-2 below: the gate is also applied inside
transactions (the `queue_or_emit` path at `exec.rs:257-280`). Comment
at `exec.rs:184-188` accepts that subscribers added mid-tx miss the
event — calling it "the conservative-true contract." This matches the
WAL consumer behaviour but it's a non-obvious user-visible semantic;
worth pinning a test that asserts it.

---

## New findings

### [MAJOR] M-NEW-1 — `OrchestratorLockGuard::Drop` returns a locked client to the pool

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:159-182`

**Symptom:**
```rust
impl Drop for OrchestratorLockGuard<'_> {
    fn drop(&mut self) {
        if !self.released {
            tracing::error!(
                key = %self.key,
                tag = %self.tag,
                "OrchestratorLockGuard dropped without release() or into_held(); \
                 advisory lock will stay held until the backend session closes"
            );
        }
    }
}
```

The `client` field is not taken; the `PooledClient`'s own `Drop` runs
during the field-by-field drop sequence and parks the still-locked
connection back into the pool. The session-scoped advisory lock travels
WITH the connection. The next `pool.get()` caller — who may have no
idea this app even exists — receives a connection holding
`pg_advisory_lock(hashtext('zs_reg:<other_app>'), hashtext('register_model'))`.
If that caller is `register_model` for a different app it BLOCKS on its
own `pg_advisory_lock(hashtext('zs_reg:<their_app>'), ...)` (different
key, no contention there), but ANY other code that ends up issuing
`pg_advisory_unlock_all()` or `DISCARD ALL` against this pooled
connection clears the lock silently — and if no such code runs the
lock survives indefinitely.

**Why it's a problem:**
- The current Drop is a tracing log, not a recovery action. Catastrophic-
  path semantics are not "best-effort cleanup"; they are
  "indefinitely-stuck cross-app lock".
- The doc block (`lock_guard.rs:36-41`) says "the session-scoped lock
  will release when the pooled connection is recycled or the backend
  session ends" — that's only true if the pool ever closes that
  connection. `compio_postgres::Pool` does not have a TTL-based
  eviction by default (verify against `compio-postgres` source).
- The fallback is supposed to be "panic unwind, missed `release()`
  call". Both are real production scenarios (a `?` propagation skipping
  a `release()` would also hit this — and indeed the comment at
  `register_model/mod.rs:178-198` explicitly motivates the guard as
  fixing exactly that bug pattern).

**Fix options:**
1. Drop the client AND issue a synchronous `pg_advisory_unlock_all`
   via blocking SQL on the way out. The Drop is sync so the unlock
   has to be blocking — but the pool task is async. Not trivial.
2. Drop the client entirely on Drop instead of returning it to the
   pool. The pool re-opens a fresh connection on next `get()`, the
   lock dies with the session.
3. Mark the connection as "tainted" so the pool closes it instead of
   recycling. Requires `compio-postgres` API support.

Option (2) is the cleanest: `let _ = self.client.take().map(drop);`
inside the `Drop` would close the connection at the OS level when its
underlying socket drops. The pool's connection-task `select!` should
observe the close. Verify against `compio_postgres::PooledClient`.

**Verification:**
- `grep -n "fn drop" crates/plugin-db/src/orchestrator/lock_guard.rs`
- `grep -nC5 "impl<'p>.*PooledClient" crates/compio-postgres/src/` — confirm what `PooledClient::Drop` does.

### [MAJOR] M-NEW-2 — `mint_subscription` leaks broker entry on V8 alloc failure

**File:** `crates/plugin-db/src/v8_classes/subscription.rs:157-208`

**Symptom:**
```rust
pub fn mint_subscription<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    app_id: &str,
    collection: &str,
) -> Result<v8::Local<'s, v8::Object>, OpError> {
    // Register on the broker first — failing the V8 allocation after
    // the broker registration would leak a broker entry. The
    // wrapper-allocation path below never registers a broker entry
    // until it has the JS object in hand.
    let broker_sub = broker::subscribe(app_id, collection);

    let class_tmpl = Subscription::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Subscription instance allocation failed"))?;
    // ...
```

The comment at L162-165 is exactly backwards. It claims "Register on the
broker first" prevents a leak, but `broker::subscribe(app_id, collection)`
DOES register the entry first; if `new_instance` returns `None`, or
`get_function` / `get(proto_key.into())` return `None`, the `?` propagates
the error and the `broker_sub` (which is `Rc<RefCell<Inner>>`) is dropped.
Dropping `broker_sub` only decrements the refcount — the broker's own
clone (`subs.push(sub.clone())` at `broker.rs:441`) keeps the entry
alive in the routing table indefinitely. `is_closed()` returns `false`
because `close()` was never called.

**Why it's a problem:**
- Repeated mint failures (e.g. an isolate under V8 heap pressure)
  accumulate dead broker entries that the `subs.retain(|s| !s.is_closed())`
  pass at `broker.rs:492` will never prune.
- `BrokerSubscription` is `Clone` — dropping the local clone is a no-op
  for the broker's clone. Only an explicit `close()` releases the slot.
- The doc comment dismisses this scenario; the actual leak is real.

**Fix:**
Either acquire the V8 object first then subscribe, or wrap the broker
subscription in an RAII guard whose Drop calls `close()`:
```rust
struct SubscriptionGuard(Option<BrokerSubscription>);
impl Drop for SubscriptionGuard {
    fn drop(&mut self) {
        if let Some(s) = self.0.take() { s.close(); }
    }
}
let mut guard = SubscriptionGuard(Some(broker::subscribe(...)));
let obj = inst_tmpl.new_instance(scope).ok_or(...)?;
// ... all the fallible setup ...
let broker_sub = guard.0.take().unwrap(); // disarm
let state = Subscription { inner: RefCell::new(Some(broker_sub)) };
```

Once `Box::into_raw` succeeds and the V8 finalizer is registered, drop
the guard without re-closing.

**Verification:**
- `crates/plugin-db/src/v8_classes/subscription.rs:166-208`
- `grep -n "subs.retain" crates/plugin-db/src/broker.rs` — confirm the
  prune predicate (`!s.is_closed()`).

### [MAJOR] M-NEW-3 — `OrchestratorLockGuard::into_held` is dead and unenforced

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:132-156`

```rust
#[allow(dead_code)]
pub(crate) fn into_held(mut self) -> PooledClient<'p> {
    self.released = true;
    self.client
        .take()
        .expect("OrchestratorLockGuard::into_held called on guard with no client")
}
```

`grep into_held crates/plugin-db` returns one file (lock_guard.rs
itself). The doc block (`lock_guard.rs:32-35`, `lock_guard.rs:132-143`)
justifies it as "kept codified at the guard boundary rather than
re-discovered". I accept the rationale; the cost is that:

1. The function signature returns a raw `PooledClient<'p>` with no
   token / receipt — there is no compile-time check that the caller
   eventually issues `pg_advisory_unlock`. A future caller that grabs
   the client and forgets the unlock obtains the same M-NEW-1 leak
   without the tracing warning.
2. `.expect()` at L155 has no `released` guard; if a test (or a future
   refactor) constructs a guard via the test helper
   `for_test_no_client` and calls `into_held`, the call panics. A
   `Result<PooledClient<'p>, DbError>` return would force callers to
   acknowledge the failure mode.

**Fix:**
Either delete `into_held` (clearer signal: "the current pipeline uses
`release()` only; design a new API when a real handoff caller arrives")
or change the signature to `Result<PooledClient<'p>, OrchestratorError>`
where `OrchestratorError::NoClient` is distinct from
`OrchestratorError::AlreadyReleased`, removing the `.expect()`.

**Verification:**
- `grep -rn "into_held" crates/plugin-db/` — confirm zero callers.

### [MINOR] M-NEW-4 — `Subscription::push` calls `waker.wake()` while RefCell is borrowed

**File:** `crates/plugin-db/src/broker.rs:306-328`

```rust
pub fn push(&self, msg: SubscriptionMessage) {
    let mut inner = self.0.borrow_mut();
    if inner.closed {
        return;
    }
    if inner.queue.len() >= inner.max_queue {
        if !inner.resync_pending {
            inner.queue.clear();
            inner.queue.push_back(SubscriptionMessage::Resync);
            inner.resync_pending = true;
        }
        if let Some(w) = inner.waker.take() {
            w.wake();
        }
        return;
    }
    inner.queue.push_back(msg);
    if let Some(w) = inner.waker.take() {
        w.wake();
    }
}
```

`inner` (a `RefMut<SubscriptionInner>`) is alive when `w.wake()` is
called. If a `Waker::wake_by_ref` impl ever synchronously invokes a
poll that re-enters `Subscription::pop` / `register_waker` / `is_closed`
on the SAME subscription, the inner `borrow_mut` panics. Today's
compio waker just schedules the task and returns — so this doesn't fire
in production. But the pattern is fragile to:
- a future custom executor that polls inline,
- a test harness that drives wakes synchronously,
- a `Waker` produced by `noop_waker` or similar that someone composes
  with a synchronous side-effect.

**Fix:**
Drop the borrow before waking. Idiomatic Rust:
```rust
let waker = inner.waker.take();
drop(inner);
if let Some(w) = waker { w.wake(); }
```
This is a one-line refactor with no semantic change in the common case
and panic-immunity against the hostile case.

**Verification:**
- `grep -nC3 "waker.take" crates/plugin-db/src/broker.rs`
- `grep -rn "impl Waker" crates/plugin-db/ crates/runtime/ crates/compio-postgres/` —
  confirm none re-enter synchronously today.

### [MINOR] M-NEW-5 — `run_sql` is not cancellation-safe with tx_conn slot

**File:** `crates/plugin-db/src/exec.rs:43-73`

```rust
pub(crate) async fn run_sql(
    sql: &str,
    params: &[&str],
) -> Result<Vec<compio_postgres::Row>, DbError> {
    let has_tx = context::with(|c| c.has_tx());
    if has_tx {
        let client = context::with_mut(|c| c.take_tx_client())
            .ok_or_else(|| DbError::internal("db: transaction connection lost"))?;
        let result = client.query_text_params(sql, params).await;
        // Put it back
        context::with_mut(|c| c.put_tx_client(client));
        return result.map_err(|e| DbError::from_pg(&e));
    }
    // ...
}
```

The shape `take → await → put` is NOT cancellation-safe. If the future
holding this `run_sql` is dropped while the `.await` is parked
(`spawned_ops.push(...)` future cancelled by isolate shutdown, by a
race in the dispatcher, by a panic in another task), `client` is
dropped without being returned to the slot. Subsequent `run_sql` calls
under the same transaction see `take_tx_client() == None` and the
`ok_or_else` arm fires `"db: transaction connection lost"` — which
sounds like a recoverable error but is actually a permanent silent
drop. The transaction is also rolled back at Postgres because the
connection task observed the Sender close.

**Why it's a problem:**
- The "lost connection" path returns `DbError::internal` (a plain
  `Internal` variant) — the SDK can't distinguish it from a Postgres
  failure mid-call.
- Once it fires, the wrapper's `tx_token` is still set, so the
  Transaction wrapper's `commit()` / `rollback()` see
  `take_tx_client() == None` and silently no-op (`v8_classes/transaction.rs:237-243`).
  Net effect: an in-flight tx vanishes without an audit-visible event.

**Fix:**
RAII guard for the take/put round-trip:
```rust
struct TxClientGuard<'a>(Option<Client>, &'a SharedState);
impl Drop for TxClientGuard<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            context::with_mut(|ctx| ctx.put_tx_client(c));
        }
    }
}
```
Then `let mut guard = TxClientGuard(Some(client), &state);` and refer
to `guard.0.as_ref().unwrap()` for the await; on cancellation the
client is put back automatically.

**Verification:**
- `crates/plugin-db/src/exec.rs:48-57`
- `crates/plugin-db/src/v8_classes/transaction.rs:237-243` (the Drop /
  commit silent-no-op path)

### [MINOR] M-NEW-6 — `into_held` doc comment self-contradicts on no-callers status

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:132-143`

The doc block says "In the current pipeline the bootstrap → apply
boundary keeps the guard itself in scope (no need to drop down to the
raw `PooledClient`). This method exists for future callers..." — but
the `#[allow(dead_code)]` immediately under it (L143) confirms there
ARE no current callers. The comment reads as if the method has some
in-pipeline use; it does not. Either delete the method (M-NEW-3) or
rewrite the comment to start with "Currently unused; retained because…"

### [MINOR] M-NEW-7 — `release_idempotent_when_no_client` test creates a fresh compio runtime per call

**File:** `crates/plugin-db/src/orchestrator/lock_guard.rs:202-215`

```rust
let out = compio::runtime::Runtime::new()
    .unwrap()
    .block_on(async move { guard.release().await });
```

The `.unwrap()` is a real panic site if compio rejects the runtime
construction (out of FDs, io_uring not available in the test env).
Other tests in the crate use `crate::test_util::*` or similar fixtures;
this is one-off. Migrate to `#[compio::test]` or a shared `block_on`
helper.

---

## R3 carry-over status

### I1 — `release_advisory_lock` void return type

**Status: superseded.** The new `OrchestratorLockGuard` (`cbd12944`)
replaces every inline `release_advisory_lock(client, key).await; drop(client);`
sequence with `guard.release().await?`. The trait method's void return
is no longer in the hot path; only the `Backend::acquire_advisory_lock`
half stays. R4 closes this item by architectural displacement.

### I2 — `Result<_, String>` site count

**Status: open, partial movement.**

Count: 45 occurrences across 13 files (`grep -rn 'Result<.*,\s*String>' crates/plugin-db/src`).

Largest remaining offenders:
- `auth/bootstrap.rs`: 13 sites — pre-platform-bootstrap helpers; lower priority.
- `replication.rs`: 8 sites — the reactive-queries surface; one
  `into_string()` adaptor at L248 already imports the typed-error
  shape, but every helper still propagates as `String`.
- `auth/session.rs`: 6 sites.
- `diff.rs`: 3 sites.
- `auth/keys.rs`: 3 sites.

The `validate.rs` lone `Result<_, String>` (`validate.rs:57`) is
documented and load-bearing (the `validation_refused` JSON envelope is
an SDK wire contract) — out of scope for the sweep.

### I3 — `replication.rs` lsn=`""` mirror

**Status: closed by `c83d6a8c`.** Verified above.

### M1 — `v8_bridge.rs:217` `try_into().unwrap()`

**Status: still open.**

```rust
crates/plugin-db/src/v8_bridge.rs:217:    let arr: v8::Local<v8::Array> = v.try_into().unwrap();
```

Single line; not yet rewritten. The `try_into` here is on a
`v8::Local<v8::Value>` that the caller has already verified is an
array via context, so the unwrap is "should be unreachable" — but a
future caller refactor could break that invariant silently. Rewrite as
`let Ok(arr) = v.try_into() else { return Value::Null; }`.

### M2 — `v8_bridge.rs:171` i64-bound off-by-one

**Status: not re-verified this round.** Carried forward.

### M5 — `migrations.rs` silent `finalise_backfill`

**Status: not re-verified this round.** Carried forward.

---

## Audit summary — eight dimensions

| Dimension | Finding | Severity |
|---|---|---|
| 1. RefCell-across-await | `broker.rs::publish` itself is fine — no `.await` between `borrow_mut` and end of scope. The `.wake()` re-entry risk on `push` (M-NEW-4) is latent, not active. `lock_guard.rs::release` awaits with `client` taken (no RefCell). `subscription.rs::next` snapshots `BrokerSubscription` clone before the `poll_fn` await (`subscription.rs:100`) — clean. | MINOR |
| 2. Unsafe | 6 finalizer-pattern `unsafe { drop(Box::from_raw(addr as *mut T)) }` blocks across v8_classes (`db.rs:402`, `migration.rs:578`, `migration.rs:689` — UAF-adjacent at the post-Promise-resolve site, gated by the `migration_global` keepalive; `migrations.rs:282`, `collection.rs:386`, `subscription.rs:201`, `transaction.rs:329`, `replication.rs:148`). All are correctly paired with `Box::into_raw` at construction and have SAFETY comments naming the keepalive Global. No regressions vs R3. | OK |
| 3. Panic risks | Production-path `.unwrap()` count is unchanged: 17 V8 `v8::String::new(scope, ...).unwrap()` calls (these are infallible in practice — empty-string allocation never fails in non-OOM scenarios but the unwraps could be `.unwrap_or_else(...) ` for safety in the hostile case). New panic site: `lock_guard.rs::into_held::expect` (M-NEW-3). | MINOR |
| 4. Error handling typing | `Result<_, String>` count: 45 across 13 files. Down from R3's "~50" — the `c83d6a8c` typed RETURNING contributed; ~2 sites moved. Significant migration still pending. | MINOR (chronic) |
| 5. Lifetimes | `OrchestratorLockGuard<'p>` propagates a single `'p` lifetime through `acquire` / `release` / `into_held` / `bootstrap` return / `apply` parameter. The pipeline (`bootstrap → run_pipeline → apply`) threads it cleanly without explicit annotation outside the guard's own impl block — Rust infers correctly. No higher-rank bound abuse, no `for<'a>` lifting. | OK |
| 6. Idiomatic patterns | `auto_tx.rs::begin_to_resolve_value` / `end_to_resolve_value` (`auto_tx.rs:120-139`) — exemplary: small total functions, pure match, unit-tested without V8. `validate.rs::validate` uses `if !destructive.is_empty()` rather than `Vec::contains`-style — fine. The `let Some(by_collection) = ... else { return; }` let-else pattern in `broker.rs::publish` is current Rust idiom; good. | OK |
| 7. Resource lifecycle | `Subscription::Drop` (`subscription.rs:55-66`) — clean idempotent close. `Transaction::Drop` (`transaction.rs:117-138`) — token-fenced, clears the slot, drops pending emits; the `crate::context::with_mut` inside `Drop` is synchronous so no await safety concern. `Migration::Drop` (`migration.rs:84-131`) — spawns best-effort cancel under `panic::catch_unwind` to survive a mid-shutdown isolate; good defense. `OrchestratorLockGuard::Drop` — see M-NEW-1. | MAJOR (lock_guard) |
| 8. Type ascription | `begin_to_resolve_value` / `end_to_resolve_value` signatures clean. No turbofish abuse, no `as` casts in production paths (only in V8 `usize` plumbing for the finalizer pattern). | OK |

---

## Score breakdown

| Dimension | R1 | R2 | R3 | R4 | Change |
|---|---|---|---|---|---|
| Correctness | 72 | 80 | 84 | 84 | M-NEW-1 (Drop returns locked client) + M-NEW-2 (broker leak) offset auto_tx + RETURNING wins; net flat. |
| Performance | 84 | 84 | 84 | 86 | Broker two-level HashMap eliminates `(String, String)` alloc on every publish; `has_subscribers` is now O(2-bucket-probe) zero-alloc; `exec_mutation_with_emit` gate avoids tuple-build when no subscribers. |
| Security | 88 | 88 | 90 | 90 | No regression. R3 fixes hold. |
| API design | 76 | 82 | 84 | 86 | `OrchestratorLockGuard` is a real improvement — centralised invariant, three call sites collapsed to one type. `into_held` dead-code is a minor blemish. |
| Rust idioms | 80 | 84 | 86 | 87 | Auto-tx typed-error helpers are exemplary; `let-else` in publish; `?` propagation reaches the typed rail at more sites. `Result<_, String>` still chronic. |
| **Overall** | **78** | **85** | **87** | **88** | Net +1: two MAJOR-class findings unmasked by new RAII abstraction are real, but offset by genuine improvements across performance + API design + idioms. |

---

## Ceiling-blockers for 90+

In priority order:

1. **M-NEW-1** — `OrchestratorLockGuard::Drop` parks a still-locked
   client back in the pool. Until this is fixed (close the connection
   on Drop, or taint it for pool eviction), the production-path
   guarantee the guard claims is incomplete. Single-file fix.

2. **M-NEW-2** — `mint_subscription` leaks broker entry on V8 alloc
   failure. Two-line RAII guard wrap.

3. **I2 carry-over** — `Result<_, String>` count = 45. Each migration
   removes a `.into_string()` adaptor + restores `.code` discrimination
   at the SDK boundary. `replication.rs` (8 sites) and `auth/session.rs`
   (6 sites) are the highest-leverage targets.

4. **M-NEW-5** — `run_sql` take/put round-trip is not cancellation-safe.
   RAII guard for the tx_conn slot.

5. **M-NEW-4** — `Subscription::push` waker-wake-while-borrowed pattern.
   One-line drop-the-borrow fix; defense-in-depth.

Items below the cut (M-NEW-3 / M-NEW-6 / M-NEW-7) are cosmetic and
don't affect the score meaningfully.

---

## Verification commands

```bash
# RefCell-across-await sweep
grep -rn "borrow_mut\|borrow()" crates/plugin-db/src | wc -l

# Unsafe blocks
grep -rn "unsafe" crates/plugin-db/src

# Panic sites (production paths only — tests excluded by file path)
grep -rn "\.unwrap()\|\.expect(" crates/plugin-db/src | grep -v "/tests/" | grep -v "mod tests"

# Result<_, String> remaining
grep -rEn "Result<.*,\s*String>" crates/plugin-db/src | wc -l

# Lock guard call sites
grep -rn "OrchestratorLockGuard\|into_held\|guard.release" crates/plugin-db/src

# Auto-tx typed-error helpers
grep -nC3 "begin_to_resolve_value\|end_to_resolve_value" crates/plugin-db/src/orchestrator/auto_tx.rs

# Broker bucket layout
grep -nC5 "by_key" crates/plugin-db/src/broker.rs
```
