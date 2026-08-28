# SC-1: the explicit transaction protocol

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** step 9 of that document (the owned transaction registry and state
machine) and the randomized-encryption atomicity that depends on it.

It no longer gates the L8 fix: that landed on main in `42c925de7` while this
document was being written, so the settle path already judges its terminal
command tag. This contract inherits that behaviour rather than introducing it.

---

## Why this is a document and not a discovery

Round 3 established that a black-box test suite **underdetermines** this
protocol: a suite that never states whether two same-app top-level transactions
serialise passes just as happily on either answer. And the answer is
user-visible - it decides what
`Promise.all([db.transaction(a), db.transaction(b)])` does.

The existing code already made that choice, deliberately, and says so
(`crates/zeroship-plugin-db/src/transaction/mod.rs:354-361`):

> Serialise top-level transactions for this app on this isolate. Only ONE tx
> connection slot exists per (app, isolate), so a second concurrent top-level
> transaction has nowhere to live: it used to evict the first (Postgres) or be
> refused by the single SQLite writer... Waiting turns both of those into "runs
> second and succeeds", which is what a creator writing
> `Promise.all([db.transaction(a), db.transaction(b)])` means.

**That behaviour is preserved.** This contract does not silently delete it, and
the parent proposal's earlier claim that "every transaction slot and claim" can
be keyed by `(runtime_instance_id, tx_id)` is withdrawn for the claim half:
unique transaction ids never contend, so keying admission by them would remove
the serialisation above without anyone deciding to.

## Two identities, deliberately distinct

| Identity | Value | Purpose |
| --- | --- | --- |
| **Registry identity** | `TxKey(runtime_instance_id, tx_id)` | Which transaction is this? Indexes the state entry, the savepoint frames, and the pending-effect buffer. Unique per transaction, so it never contends. It needs no incarnation or domain: a `tx_id` is minted per transaction and never outlives the app instance that created it, so it cannot alias across incarnations the way a durable key can. |
| **Admission identity** | PostgreSQL: `(runtime_instance_id, app_id, incarnation)`. SQLite: `(thread_resource, app_id, incarnation)` | May this transaction *start*? A second waits. Preserves the documented `Promise.all` semantics. **The key differs by backend, and that is Fork A's decision rather than an oversight**: SQLite serializes across isolates sharing one actor, because SC-2 gives an attached app file exactly one transaction connection. The `incarnation` component is Fork C's - without it a queue entry created by the previous app instance can be admitted against the new one. **The authority domain is not a key component here**, and invariant 1's domain-mismatch denial is not weakened by that: admission is process-local and short-lived, so it cannot outlive a timeline change the way a durable row or a cached entry can. The domain is compared where it can actually differ - at the authority read, against the binding's captured value. |

The comment quoted above says the slot is per `(app, isolate)`, but the code
keys it by `app_id` alone - `take_tx_client_for(app_id)`
(`transaction/mod.rs:1029`), `TxClientSlotGuard::take(route.app_id())`
(`exec.rs:390`). That gap is the cross-isolate defect the parent proposal fixes:
the *intent* was always per-isolate. Adding `runtime_instance_id` to both keys
makes the code match the comment.

## States

```text
                    admission granted
     Starting ------------------------------> Idle
        |                                    ^   |
        | BEGIN fails                        |   | an operation starts
        | or cancelled                       |   v
        |            operation completes ----+  InFlight
        |                                    |     |
        |          ROLLBACK TO SAVEPOINT ----+     | a statement errored
        |          (nested reject; the             v
        |           outer transaction           Poisoned
        |           continues)                     |
        |                                          | settle requested
        |            settle requested              | (COMMIT here is legal
        v                    |                     |  and FAILS: the server
     Settled <--- Settling <-+---------------------+  answers ROLLBACK)
```

Settlement may also be requested directly from `Idle` or `InFlight`; from
`InFlight` it waits for the operation to return the client rather than treating
the empty slot as proof that terminal SQL ran.

- **Starting** - admission granted, `BEGIN` issued, no client installed yet.
- **Idle** - transaction open, no operation owns the client.
- **InFlight** - an operation owns the client. **Settlement may arrive here.**
- **Poisoned** - a statement errored. PostgreSQL refuses every further *data*
  statement until the transaction ends
  (`libs/compio-postgres/src/client.rs:507-533`), so this is a real server-side
  state, not a bookkeeping flag.

  **A `COMMIT` from `Poisoned` is reachable and must be handled, not forbidden.**
  An earlier draft said "only `ROLLBACK` is legal" and then, four lines later,
  described a `COMMIT` being sent from this state - a contradiction. The
  reachable truth: a creator callback can swallow the error and resolve, so the
  orchestrator issues `COMMIT`; PostgreSQL accepts it and answers with the tag
  `ROLLBACK`. The transition is legal, its outcome is **failure**, and the
  machine records it as such. That is exactly the L8 case, now covered by
  `commit_that_postgres_rolled_back_must_not_report_success_l8`.

  A **savepoint** rollback does *not* leave `Poisoned` terminal: `ROLLBACK TO
  SAVEPOINT` returns the transaction to `Idle`, which the tree implements and
  tests (`crates/zeroship-plugin-db/tests/native_transaction.rs:671-705`, the
  nested-inner-reject case where the outer transaction continues and commits).
  The state diagram carries that arc; without it the machine would forbid a
  behaviour the product ships.
- **Settling** - terminal SQL has been issued and not yet answered.
- **Settled** - terminal, with a recorded outcome.

`Poisoned` is why five labels were not enough.

## The rules that are actually load-bearing

1. **An absent client is never proof that terminal SQL ran.** Today's settle
   path does exactly that: `take_tx_client_for` returning `None` falls into
   "Slot already drained ... Treat as settled", releases the claim, clears
   pending emits, and returns `SettleOutcome::Ok` **without sending anything**
   (`transaction/mod.rs:1034-1039`). That is DBR-03. Under this contract, a settle
   arriving while the state is `InFlight` **waits for the operation to return
   the client**; only a state of `Settled` ends a settle early.

2. **Terminal SQL inspects its command tag** - already true, and this contract
   must not regress it. A `COMMIT` answered `ROLLBACK` is a failed transaction,
   not a successful one. Before `42c925de7` the raw path discarded the tag and a
   rolled-back transaction was reported to the creator as committed; that is now
   fixed and covered by
   `commit_that_postgres_rolled_back_must_not_report_success_l8`. The check is
   scoped to the PostgreSQL `COMMIT` arm deliberately: `RELEASE` answers with
   the tag `RELEASE`, so a broader "anything but COMMIT is a failure" test would
   reject every healthy nested commit.

3. **Pending effects are discarded unless the commit is confirmed** - which is
   already the behaviour, and this rule exists to keep it rather than to fix it.
   An earlier draft claimed the early-return path "publishes change events for
   writes the database discarded". That is **false**: `clear_pending_emits` is
   documented as clearing the queue "without firing any events"
   (`crates/zeroship-plugin-db/src/exec.rs:615-621`), and every settle arm other
   than `(true, Ok)` calls it. The queue is dropped, not published.

   The rule still belongs here, because the state machine must not *introduce*
   the defect: an effect buffer keyed per transaction, published only from a
   confirmed-commit arm, is what preserves today's behaviour once settlement
   stops being a single early-return.

4. **The deadline is enforced by an independent timer, not by the settle path.**
   The parent proposal's earlier wording ("a deadline enforced by the settle
   path") was circular: a body that never settles never reaches the settle path.
   The timer is armed at `Starting` and fires regardless of callback behaviour,
   moving the transaction to `Poisoned` and then `Settling` with a rollback.

5. **Cancellation before `BEGIN` returns must not leak the admission claim.** An
   RAII guard is armed at admission and disarmed only once the client is
   installed. Today the claim can outlive the isolate, leaving later
   transactions for that app parked indefinitely (DBR-11).

## What this contract does not decide

- Whether SQLite's actor can honour the same states. That is **SC-2**, and it
  must land first for the SQLite arm: cancellation and rollback here assume an
  actor that can be interrupted, which today's cannot.
- The plan-level atomicity that randomized encryption needs (DBR-04/05). That
  depends on **SC-3**'s grammar.

## Frames and effects

The transaction owns a **strict-LIFO frame stack**. Only the innermost open
frame may issue data SQL, open a child, or close. The root frame is created
only once `BEGIN` is confirmed; a child is inserted as `Opening` before
`SAVEPOINT` is sent and becomes `Open` only when that command succeeds.

**Effects are per-frame, not per-app**, and their fate is a property of how the
frame closed:

| Frame closes by | The frame's queued effects |
| --- | --- |
| `RELEASE` succeeds | appended to the parent, in order. Nothing is published |
| `ROLLBACK TO` succeeds | **discarded** - the database changes they describe are known to be undone |
| `ROLLBACK TO` fails | retained for diagnosis; the transaction poisons; nothing is published |
| `ROLLBACK TO` succeeds but the following `RELEASE` fails | effects **stay discarded**; an empty child frame remains; root cleanup is forced |
| confirmed **root** commit | detached and published |
| every other terminal outcome | every frame buffer discarded |

The release-after-rollback row is not padding, and omitting it was a real gap:
it is the edge that decides **whether the frame still exists**. A rolled-back
frame is not closed until its `RELEASE` lands, so until then the transaction is
not `Idle` and the frame is not available for new work - a point the state table
must agree with rather than reporting `Idle` the moment `ROLLBACK TO` returns.

**Ordering is contract, not implementation detail.** The frame's fate is applied
**after** the statement succeeds, never before. Discarding on the assumption
that `ROLLBACK TO` will succeed makes the failure row above unachievable - the
diagnostic evidence it calls for is already gone by the time the failure is
known. The shipped code did exactly that and has been corrected: the settle path
now takes the frame's watermark, runs the statement, and only then applies the
fate.

This is not speculative. A flat, app-keyed effect buffer with no frame scoping
was the shipped behaviour, the nested settle arm never touched it, and the
top-level `COMMIT` drained all of it - so a subscriber was told about a row that
had been rolled back and did not exist. That defect is now **proven and fixed**
(`fix(db): discard a rolled-back savepoint's queued change events`), with a
regression test that fails on the pre-fix code. The shipped fix is the minimal
form of this table - a watermark per frame into one buffer; the per-frame `Vec`
above is the same contract with a cleaner representation.

### Savepoint names are monotonic, never depth-derived

A frame's savepoint name comes from a **monotonic sequence**, not from the
current depth. The simultaneous-open depth stays capped at eight, which is the
existing public limit - the sequence governs naming only.

The reason is a server behaviour the driver already documents
(`libs/compio-postgres/src/transaction.rs:63-82`): `ROLLBACK TO SAVEPOINT`
deliberately **leaves the savepoint defined**, and PostgreSQL resolves a
savepoint name to the **most recently established** one. A depth-derived name
(`zs_sp_<N>`) is therefore reused after the depth decrements, so a leftover
savepoint shadows an enclosing frame of the same name and sends the enclosing
rollback **to the wrong scope**. It also leaves one open subtransaction per
rolled-back savepoint, which a retry loop accumulates.

`ROLLBACK TO` must accordingly be followed by `RELEASE` of the same name, and
the order is not interchangeable: after a failed statement the subtransaction is
in an aborted state where `RELEASE` is refused and only `ROLLBACK TO` recovers
it. Monotonic naming is the stronger half of the fix - it removes the shadowing
precondition outright rather than relying on every cleanup path succeeding.

The orchestrator today sends only `ROLLBACK TO`, with depth-derived names
(`crates/zeroship-plugin-db/src/transaction/mod.rs:988-1002`), so both halves
are live defects rather than hypotheticals.

## The invariants a property test asserts

The state table says what each transition does; these say what must hold after
**every** generated prefix, including prefixes ending in an illegal event. A
pure reducer driven by a model backend is what makes them checkable without a
database.

1. **Qualified identity.** An event for authority A never mutates, interrupts,
   settles or executes SQL for B. Epoch mismatch re-resolves; domain or
   incarnation mismatch denies terminally.
2. **Admission cardinality.** At most one nonterminal entry owns an admission
   key - which contains the runtime instance on PostgreSQL and the thread
   resource on SQLite, per Fork A.
3. **Claim balance.** Every granted admission has exactly one release, and only
   once no live transaction or reservation remains.
4. **Session conservation.** Between confirmed `BEGIN` and terminal cleanup,
   session ownership is exactly one of the registry, the matching command token,
   or quarantined - never silently absent.
5. **Single command.** At most one active backend token per transaction;
   duplicate or stale completions cannot alter state or effects.
6. **Operation serialization.** A second operation while one is in flight
   returns `transaction_connection_busy` - never silently deferred, never
   silently autocommitted.
7. **Authority separation.** Every data-SQL trace is preceded by a successful
   platform-role authority read for the same app authority, and **no authority
   read is ever on the tenant data session** (Fork B).
8. **Ceiling monotonicity.** The effective ceiling is never broader than the
   `BEGIN` ceiling or any previously accepted value: a mid-transaction raise
   changes nothing, a lower value tightens the next authorization.
9. **Frame stack.** Root at index zero, parent links form one chain, only the
   top acts, simultaneous child depth at most eight, and **a frame sequence or
   name never repeats**.
10. **Complete child close.** Every child that closes normally has exactly one
    `RELEASE`; a rolled-back child has `ROLLBACK TO` **before** `RELEASE`.
11. **Effect locality.** Success appends only to the current frame; release
    moves the exact child sequence to the parent; a confirmed rollback-to
    discards exactly the child sequence **and no parent effect**.
12. **Commit-only publication.** Nothing publishes unless the root committed,
    and a confirmed commit publishes every retained effect exactly once, in
    order.

Invariant 11 is the one the shipped fix already leans on: when a frame's
watermark is missing the buffer is left alone rather than truncated to zero,
precisely because truncating would discard parent effects. Over-publishing is a
bug; silently dropping a committed row's event is a worse one.

## Acceptance shape

A state table plus one test per illegal transition, and explicitly:

- **a FAILED `ROLLBACK TO` retains the frame's effects**, asserted by faulting
  the rollback statement after an effect is queued: the child effects and the
  frame remain diagnostically present, nothing publishes, the transaction
  poisons, and root cleanup finally discards them.

  This arm exists because the success-path arm below **cannot fail against a
  premature discard** - which is not hypothetical, it is what the shipped code
  did until `5b9bcbd49`. The successful-rollback regression test was green the
  whole time the failure path was destroying the evidence this contract requires
  it to keep. A table with two rows needs an arm for each row; asserting only
  the row that already passes measures nothing about the other.
- **a rolled-back frame's effects are never published**, asserted with a live
  subscriber present. The subscriber is load-bearing: the emit path returns
  early unless `broker::has_subscribers(app, collection)`
  (`crates/zeroship-plugin-db/src/exec.rs:501-504`), so without one nothing is
  ever queued and the arm passes vacuously **against the very defect it exists
  to catch**. This arm is green today and its regression test is committed.
- **a savepoint name is never reused while a leftover of that name can exist on
  the server**, asserted by driving a rollback and then opening a new frame at
  the same depth. A depth-derived name fails this; a monotonic one passes.
- a second same-app top-level `begin` **waits** and then succeeds, with both
  transactions' writes durable - the `Promise.all` case named in the source -
  **and the test asserts the exclusion directly, not merely that both writes
  landed.**

  The durability half **cannot fail**: the serialization already exists and is
  deliberate today (`transaction/mod.rs:354-361`, plus `tx_claims` at
  `context.rs:182`, "held until the matching COMMIT/ROLLBACK has settled"), so
  both transactions already succeed with both writes durable. It would also pass
  on a *broken* implementation that keyed admission by `tx_id`, which removes
  the waiting while still landing both writes. The only discriminating
  observable is the mutual exclusion itself - a detectable non-overlap of the
  two critical sections - which this arm must therefore require explicitly.
  This document's own opening argues that a black-box suite cannot see this
  property; leaving the arm in the weaker form conceded exactly that point.
- two same-app transactions in **different isolates** cannot reach each other's
  client. This half is backend-neutral.
- **PostgreSQL arm only:** two same-app transactions in different isolates do
  not contend. **On SQLite they deliberately do**, and the label is the fix.

  An earlier draft stated non-contention as a backend-neutral arm, which could
  not pass on SQLite on any implementation: SC-5 has current and deploy-pinned
  isolates on one OS thread resolve the same `DbThreadResources` (`sc5:36-42`)
  and SC-2 gives each attached app file exactly one `tx_conn` (`sc2:55-61`) -
  two admitted transactions, one connection. It was a PostgreSQL property
  inherited onto a backend that cannot serve it.

  The alternative - a bounded set of SQLite transaction connections keyed by
  `TxKey` - is rejected, and both round-6 reviewers rejected it independently
  for reasons that compound:

  - the worker **refuses SQLite DSNs** outright
    (`crates/zeroship-worker/src/main.rs:105-116`), and deploy-pinned isolates
    exist only in the worker, so the SQLite tier has **one isolate per app by
    construction**. Extra transaction connections would buy concurrency that
    tier cannot produce;
  - SQLite permits one **writer** per database regardless, so they would buy
    concurrent readers and nothing more;
  - and they would trade deterministic queueing for `SQLITE_BUSY_SNAPSHOT`
    nondeterminism on write upgrade, while pinning WAL read marks.

  So SQLite serializes top-level transactions per `(thread-resource, app_id)` at
  the actor's `Reserve(Transaction)` queue. The divergence is house style rather
  than an embarrassment - `docs/reference/db.md:992-997` already disclaims
  dev-tier concurrency fidelity - but it is **stated**, not discovered by
  whoever first writes a concurrency test on the dev tier.
- a settle arriving while an operation owns the client waits, and terminal SQL
  is sent exactly once;
- a `COMMIT` answered with a `ROLLBACK` tag surfaces as a failed transaction and
  publishes no change events;
- a callback that never settles is terminated by the deadline, with the
  transaction rolled back and the claim released;
- cancellation between admission and `BEGIN` releases the claim, proved by a
  following transaction for the same app succeeding.
