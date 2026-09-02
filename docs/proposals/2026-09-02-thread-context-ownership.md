# Where `context.rs` goes: nowhere, because it is four owners

Status: proposed, 2026-09-02. Decides #100. Unblocks steps 2-3 of
`2026-09-02-sc1-vendor-lane.md` and the `transaction/` half of the split.

## The question, and why it has no answer as asked

#100 records that `docs/proposals/2026-08-31-data-crate-shape.md` places
`context.rs` in two different crates - `:147` the adapter, `:191` and `:1733`
data-engine - both worded as settled, and concludes the answer is open.

It is open because the question is malformed. A struct lives in one crate, and
this struct's fields do not belong to one tier:

| field | holds | tier |
| --- | --- | --- |
| `pool` | `Rc<compio_postgres::Pool>` | vendor |
| `tx_conns` | `OwnedPooledClient` / `SqliteSessionHandle` | vendor |
| `tx_cancellers` | `CancelToken` / `SqliteCancelHandle` | vendor |
| `transactions` | `TxReducer`, the SC-1 state machines | engine |
| `tx_claims`, `tx_waiters`, `tx_slot_waiters`, `withdrawn_tx_sessions` | admission and slot bookkeeping | engine |
| `savepoint_emit_marks`, `pending_emits` | change-event queue and watermarks | broker |
| `schemas` | descriptor cache, keyed by DEPLOY | core |
| `mask_policies` | unmask policy cache, keyed by app | engine |

Adapter makes the engine reach up for its own reducers. data-engine makes the
vendor-neutral crate link the Postgres driver, which is the one thing the split
exists to prevent. The census's own rule table is explicit that anything not
listed is a violation, and `ENGINE:compio_postgres` is not listed.

## The finding underneath it

**Nine of the eleven maps are keyed by `app_id` and describe one thing: an app's
open transaction.** They are created together, mutated together and destroyed
together. 47 call sites touch six of them.

The destructor is written by hand, across three maps:

```rust
pub(crate) fn retire_transaction(&mut self, app_id: &str) {
    self.transactions.remove(app_id);
    self.savepoint_emit_marks.remove(app_id);
    self.tx_cancellers.remove(app_id);
    self.wake_tx_slot_waiters(app_id);
}
```

Its own doc explains the hazard it exists to avoid: "Leaving the watermarks would
let the next transaction's first savepoint pop a stale mark and truncate that
transaction's buffer to an unrelated length."

That is a lifetime that must be remembered rather than one the compiler keeps.

## The decision

**One map of one struct**, in data-engine, with the vendor half opaque.

```rust
/// Everything true of one app's open transaction, in one place.
struct TxLane {
    reducer:   TxReducer,             // protocol
    session:   Option<TxSession>,     // vendor, OPAQUE to this crate
    canceller: Option<TxCanceller>,   // vendor, OPAQUE
    withdrawn: bool,
    admission_waiters: Vec<Waker>,
    slot_waiters:      Vec<Waker>,
    emit_marks:        Vec<usize>,
    pending_emits:     Vec<ChangeEvent>,
}

lanes: HashMap<AppId, TxLane>
```

Four owners, each in a crate that can hold it:

| owner | crate |
| --- | --- |
| `lanes` | data-engine |
| `mask_policies` | data-engine |
| `schemas` | data-core |
| `pool`, `backend`, `db_url`, `backend_generation` | adapter, with #147's `BackendUrl` |

`context.rs` becomes the composition root: one `thread_local!` assembling the
four. That is an adapter concern by definition, and roughly 100 lines rather
than 1,688.

## Why this is right on its own terms, not only for the split

**The claim becomes the map entry.** `tx_claims: HashSet<String>` disappears:
having a lane IS the claim. "One top-level BEGIN per app" stops being an
invariant two containers must agree on and becomes `HashMap::entry` - occupied
or vacant.

That is strictly stronger than today. The claim exists because two overlapping
`transaction()` calls "used to both read it as free" in the window before
`tx_conn` is filled. That window is `TxLane { session: None, .. }`: exactly
representable. The inverse - a session with no claim - becomes unrepresentable.

**`retire_transaction` becomes `self.lanes.remove(app_id)`**, and the stale-mark
hazard its doc warns about stops being a thing to remember.

**`session` is opaque.** The engine holds it and can only call `exec`, `settle`,
`cleanup`, `canceller`. It never matches variants, never names
`OwnedPooledClient`. Same inversion already proven twice this week -
`TxConnection::exec` (`c2bd6bd99`) and `PostgresBackend::query_roled_rows_as_json`
(`f7df8d3df`) - each of which also deleted an unreachable error arm that existed
only because the pairing was re-proved at runtime.

## The merge stays

`context.rs` folded several `thread_local!`s into one deliberately, because each
"had its own borrow/take/replace ritual; lifecycle invariants were enforced by
convention only". That reasoning holds and nothing here undoes it: still one
`thread_local!`, still one borrow discipline. What changes is that the thing
behind it is four named owners instead of eleven loose maps.

## What was measured before deciding

- **No global iteration.** Zero `.iter()` / `.values()` / `.retain()` / `for` over
  `tx_conns`, `tx_claims`, `tx_waiters`, `tx_slot_waiters`,
  `withdrawn_tx_sessions`, `tx_cancellers` or `transactions`. Every access is
  per-app, so a per-key API loses nothing.
- **The broker maps share the lane's lifetime.** `savepoint_emit_marks` is
  removed by `retire_transaction` beside the reducer and the canceller;
  `pending_emits` is drained or cleared per app at settle. They belong in the
  lane, not beside the broker.
- **`schemas` is keyed differently** - by deploy, not app, and deliberately: "A
  worker thread can hold a deploy-pinned and a current isolate of one app at the
  same time, and they have different schemas." It is not lane state and does not
  join the struct.

## CORRECTION, from building it the same day

**"The claim brackets the session" does not hold on every reachable path, and
the design above rests on it.** The implementation was written, compiled clean
and passed all 711 lib tests - then broke two live arms that pass at HEAD:

```
sc1_driver::a_cleanup_that_outlived_its_transaction_leaves_the_slot_alone
sc1_driver::a_withdrawn_session_never_comes_back_from_the_pool
```

Both production callers of `release_tx_claim` remove the session first -
`Action::ReleaseAdmission` runs after the reducer emitted `ReleaseSession` or
`WithdrawSession`, and `TxAdmission::drop` calls `withdraw_tx_session` before
releasing. But `probe::abandon_reducer` retires and releases with a session
still parked, which is the state the first arm exists to test: a stale cleanup
"must not send ROLLBACK to whatever session it finds in the slot - the
transaction there is still open, and in production it would belong to the NEXT
caller". Making the entry the claim drops that session, the pool rolls it back,
and the assertion fails on backend state.

Reverted rather than patched: the available fix was to edit the probe so my own
refactor passed, on an arm guarding a cross-tenant session hazard.

**The open question this leaves, which must be settled before a second attempt:**
is "the claim released while a session is parked" a state to make
unrepresentable, or one to support? If the former, `release_tx_claim` should
refuse loudly rather than silently destroy, and the probe models something
production cannot reach. If the latter, the lane cannot own the session outright,
the entry cannot be the claim, and the central argument above has to be
re-derived.

Everything else in this document survives: the nine maps are still one entity,
the destructor is still hand-written, and the placement question is still
downstream of modelling it.

## Cost

- 55 accessors rewritten, most mechanically
  (`c.take_tx_client_for(app)` -> `c.lane_mut(app).take_session()`).
- `TxSession` must be designed first: steps 2-3 of the vendor-lane proposal,
  which this unblocks rather than depends on.
- It touches the transaction hot path, and the live suite that would catch a
  regression is FLAKY - measured 14 passed/10 failed then 13/11 on identical
  code, serially (#105, #143). The oracle for this work is the pure reducer and
  projection tests plus named isolation runs, never a suite total.
