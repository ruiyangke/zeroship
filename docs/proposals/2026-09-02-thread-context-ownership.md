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
| `tx_conns` | `PoolConnection` / `SqliteSessionHandle` | vendor |
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
`PoolConnection`. The same ownership inversion applies -
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

## RESOLUTION, measured

**Unrepresentable, and the two arms fail for two different reasons.** The
question above conflated them.

### The claim does bracket the session. The reducer proves it.

`Action::ReleaseAdmission` is emitted at **exactly one site in the whole
reducer** - `settle_now`, `reducer/mod.rs:1436` - and that site pushes
`WithdrawSession` (`:1430`) or `ReleaseSession` (`:1433`) unconditionally,
immediately before it. There is no second emission and no arm that skips the
disposition. The driver applies them in that order (`driver.rs:582-589`), and
`Action::ReleaseAdmission` is literally `retire_transaction` +
`release_tx_claim`, which is what `probe::abandon_reducer` reproduces.

So arm 1's state is not production's. **The arm says so itself**, and this
proposal missed it by reading the assertion instead of the doc block:

> *What this fixture substitutes, and why that is honest.* It restores the SAME
> session rather than provisioning a successor's. A genuine successor cannot be
> reached deterministically: admitting one requires the claim, releasing the
> claim is what wakes this waiter, and the waiter then resolves before the
> successor's `BEGIN` has run.

The production state is *a successor's session in the slot* - which is why the
assertion reads "in production it would belong to the NEXT caller". Today's
probe cannot build that, so it stands in the retired lane's own session.

**The lane makes the honest version constructible.** A probe that installs a
successor lane directly reaches "filled slot plus dead identity" deterministically,
without racing a real `BEGIN`. That is strictly more faithful than the
substitution, so arm 1 is repaired by making it model what it says it wants -
not by weakening it. The acceptance criterion is unchanged: deleting the
post-wait `identity.is_current(..)` check in `cancel_and_reclaim` must still
redden it.

### Arm 2 is a real design defect, and the design was wrong.

`withdrawn_tx_sessions` is **not lane state and cannot be**. Measured:

| site | what it does |
| --- | --- |
| `context.rs:1139` `withdraw_tx_session` | inserts the tombstone |
| `context.rs:1071` `admit_transaction` | removes it - the NEXT admission |
| `context.rs:1120` `retire_transaction` | **does not touch it** |
| `context.rs:949` `put_tx_client_for` | destroys a returning session if set |

Its lifetime deliberately spans the gap between two lanes: it is created when a
session is withdrawn, survives the lane's retirement, and is cleared only when a
successor is admitted. It exists *precisely when no lane exists*. The code says
this outright at `:1071` - "**the tombstone belongs to the session that was
withdrawn, not to the app**" - and arm 2 is the moment that matters: `held.
restore()` hands back a physical session after its lane is gone, and only the
tombstone stops it reaching the pool.

Folding it into `TxLane` kills it with the lane, so the restored session parks,
returns to the pool, and a withdrawn backend is handed to the next borrower.
That is the bug arm 2 exists to catch, and my refactor reintroduced it.

### The corrected shape: eight, not nine

`withdrawn_tx_sessions` stays thread-level beside `backend_generation` - which
this proposal had **already excluded for the same class of reason** ("monotonic
for the life of the thread and never reset"). The instinct was right and was not
applied twice.

```rust
lanes:      HashMap<AppId, TxLane>,   // eight maps, entry IS the claim
withdrawn:  HashSet<AppId>,           // tombstones; OUTLIVE their lane by design
backend_generation: u64,              // monotonic, thread-lifetime
```

The rule that falls out, and that is worth stating because it is what was
missed: **a tombstone cannot live inside the thing it is a tombstone for.**

### A third thing the build found: the lane needed a destructor

`TxLane` had no `Drop`, so `release_tx_claim`'s `lanes.remove` dropped whatever
session was still parked - and on PostgreSQL a plain drop is
`pool.return_client(entry)`, which republishes the lease as **idle**. A lane
released with a live session hands an open transaction to the next borrower, on
a thread that multiplexes co-resident apps.

Every settle path disposes of the session first, so this was not reachable
through the reducer. It was reachable through `probe::reset`. The fix is a
`Drop` impl that destroys rather than returns, which makes the guarantee
structural instead of a rule every caller must remember - strictly stronger than
HEAD, where the same property rests on call-site discipline.

## Outcome, landed in `f255c73a1`

Eleven `HashMap`s became three plus one `HashSet`, and the four owners are now
legible as fields rather than as an argument in a document:

| owner | fields | crate |
| --- | --- | --- |
| lanes | `lanes`, `withdrawn_tx_sessions`, `mask_policies` | data-engine |
| schema cache | `schemas` | data-core |
| backend | `pool`, `db_url`, `backend`, `backend_selection`, `backend_init_in_progress` | adapter |
| generation | `backend_generation` | adapter |

Verified: 711 lib tests, both feature configs (`--all-targets` and
`--features test-helpers`), dependents clean, and both live arms pass **and
still redden under their own stated mutations** - the identity check deleted
gives `idle` where `idle in transaction` is required; `__private_api_close`
deleted gives a pool `total_count` of 1 where 0 is required. The five remaining
`native_transaction` failures were measured at the pre-lane commit and fail
identically there (#143).

## Cost

- 55 accessors rewritten, most mechanically
  (`c.take_tx_client_for(app)` -> `c.lane_mut(app).take_session()`).
- `TxSession` must be designed first: steps 2-3 of the vendor-lane proposal,
  which this unblocks rather than depends on.
- It touches the transaction hot path, and the live suite that would catch a
  regression is FLAKY - measured 14 passed/10 failed then 13/11 on identical
  code, serially (#105, #143). The oracle for this work is the pure reducer and
  projection tests plus named isolation runs, never a suite total.
