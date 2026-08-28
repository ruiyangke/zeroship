# SC-1: the explicit transaction protocol

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Read the set from:**
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`. Defects cited here
by `L` number live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`, or, once
closed, in `docs/proposals/2026-08-26-runtime-db-binding-defects-closed.md` -
L8 is closed and resolves there.

**Gates:** step 9 of that document (the owned transaction registry and state
machine) and the randomized-encryption atomicity that depends on it.

---

## Terminal delivery does not survive process death

**Terminal delivery is an in-memory gate. There is no durable transaction
registry and no durable fence-job system, and neither is a deliverable of this
contract.**

The question that decides this is who waits for a transaction's terminal
verdict across a process restart. The only candidate that could force a durable
registry is a workflow step, and the workflow layer already owns that failure
mode: durable workflows keep a journal (`StepCheckpoint`, `StepOutcome`,
`StepResult` at `crates/zeroship-control/src/cron/workflow_engine.rs:35`;
`JournalStepRecord` at `sdks/workflows/src/journal.ts:64`) and refuse I/O
outside a journaled step by construction (`journal.ts:30`, and `:32` for
timers). A creator transaction inside a workflow therefore runs inside a
journaled step, and process death is handled by replay against that journal.
Adding a durable registry here would put a second durable job system underneath
one that already exists and already owns this failure mode.

**Accepted cost, deliberately taken:** a transaction inside a workflow step is
**at-least-once**. If the step commits and the process dies before the journal
records the outcome, replay re-runs the step. That is the workflow layer's
idempotency contract and belongs in `docs/reference/workflows.md`. Solving it
here would be solving it in the wrong layer, and twice.

The decision is reversible in the cheap direction: a registry can be added
later without redesigning the reducer, whereas building one and then finding
the journal already covered the case cannot be undone as cheaply.

*Open check: only the workflow path was enumerated, as the strongest candidate.
Whether any NON-workflow durable consumer awaits a transaction terminal is
unverified.*

## Why this is a document and not a discovery

A black-box test suite **underdetermines** this protocol: a suite that never
states whether two same-app top-level transactions serialise passes just as
happily on either answer. And the answer is user-visible - it decides what
`Promise.all([db.transaction(a), db.transaction(b)])` does.

The existing code already made that choice, deliberately, and says so
(`crates/zeroship-plugin-db/src/transaction/mod.rs:354-361`):

> Serialise top-level transactions for this app on this isolate. Only ONE tx
> connection slot exists per (app, isolate), so a second concurrent top-level
> transaction has nowhere to live: it used to evict the first (Postgres) or be
> refused by the single SQLite writer... Waiting turns both of those into "runs
> second and succeeds", which is what a creator writing
> `Promise.all([db.transaction(a), db.transaction(b)])` means.

**That behaviour is preserved.** This contract does not silently delete it.

## Two identities, deliberately distinct

| Identity | Key | The question it answers |
| --- | --- | --- |
| **Registry identity** | `TxKey(runtime_instance_id, tx_id)` | Which transaction is this? |
| **Admission identity** | PostgreSQL: `(runtime_instance_id, app_id, incarnation)`. SQLite: `(thread_resource, app_id, incarnation)` | May this transaction *start*? |

**Registry identity** indexes the state entry, the savepoint frames, and the
pending-effect buffer. It is unique per transaction, so it never contends. It
needs no incarnation or domain: a `tx_id` is minted per transaction and never
outlives the app instance that created it, so it cannot alias across
incarnations the way a durable key can.

**Admission identity** decides whether a transaction may *start*; a second one
holding the same key waits. That is what preserves the documented `Promise.all`
semantics above. **Admission must not be keyed by `tx_id`.** Unique transaction
ids never contend, so keying admission by them removes that serialisation
without anyone deciding to - which is why the two identities are separate keys
rather than one.

**The admission key differs by backend, and that is Fork A's decision rather
than an oversight**: SQLite serializes across isolates sharing one actor,
because SC-2 gives an attached app file exactly one transaction connection. The
`incarnation` component is Fork C's - without it a queue entry created by the
previous app instance can be admitted against the new one. **The authority
domain is not a key component here**, and invariant 1's domain-mismatch denial
is not weakened by that: admission is process-local and short-lived, so it
cannot outlive a timeline change the way a durable row or a cached entry can.
The domain is compared where it can actually differ - at the authority read,
against the binding's captured value.

The comment quoted above says the slot is per `(app, isolate)`, but the code
keys it by `app_id` alone - `take_tx_client_for(app_id)`
(`transaction/mod.rs:1029`), `TxClientSlotGuard::take(route.app_id())`
(`exec.rs:390`). That gap is the cross-isolate defect the parent proposal fixes:
the *intent* was always per-isolate. Adding `runtime_instance_id` to both keys
makes the code match the comment.

### SQLite admission serializes, and bounded connections are rejected

The admission key's SQLite row is the whole of Fork A, and its consequence
reaches the acceptance shape: non-contention between two same-app transactions
in different isolates is a **PostgreSQL-only** arm, because on SQLite they
deliberately do contend.

Stating that arm backend-neutrally would make it unpassable on SQLite under any
implementation: SC-5 has current and deploy-pinned isolates on one OS thread
resolve the same `DbThreadResources` (`sc5:36-42`) and SC-2 gives each attached
app file exactly one `tx_conn` (`sc2:55-61`) - two admitted transactions, one
connection.

The alternative - a bounded set of SQLite transaction connections keyed by
`TxKey` - is rejected, for three reasons that compound:

- the worker **refuses SQLite DSNs** outright
  (`crates/zeroship-worker/src/main.rs:105-116`), and deploy-pinned isolates
  exist only in the worker, so the SQLite tier has **one isolate per app by
  construction**. Extra transaction connections would buy concurrency that
  tier cannot produce;
- SQLite permits one **writer** per database regardless, so they would buy
  concurrent readers and nothing more;
- and they would trade deterministic queueing for `SQLITE_BUSY_SNAPSHOT`
  nondeterminism on write upgrade, while pinning WAL read marks.

So SQLite admits **one** top-level transaction per `(thread-resource, app_id)`.
**It does not serialize the second one - there is no `Reserve(Transaction)`
queue.** A second `db.transaction()` for the same app is refused at once with
`transaction_connection_busy`
(`crates/zeroship-plugin-db/src/backend/sqlite/session.rs:688-695`, whose own
heading is "Exhaustion: refuse immediately, do not queue"). Postgres queues
instead, on the pool's `acquire_timeout`, so the two tiers differ in what a
creator observes under contention: a refusal on SQLite, a wait on Postgres.

That difference decides the shape of any arm about contention. **An arm in which
a second same-app top-level `begin` waits and then succeeds is
PostgreSQL-only**, and must be labelled so - not only the cross-isolate arms.
Whether SQLite's refusal should become a bounded wait once the deadline lands is
this contract's call to make; `session.rs:694-695` explicitly defers it here.
The divergence is house style rather than an embarrassment -
`docs/reference/db.md:992-997` already disclaims dev-tier concurrency fidelity -
but it is **stated**, not discovered by whoever first writes a concurrency test
on the dev tier.

### What this contract does not decide

- Whether SQLite's actor can honour the same states. That is **SC-2**, and it
  must land first for the SQLite arm: cancellation and rollback here assume an
  actor that can be interrupted, which today's cannot.
- The plan-level atomicity that randomized encryption needs (DBR-04/05). That
  depends on **SC-3**'s grammar.

## States

```text
               authority read: Current          BEGIN confirmed
     Preparing ----------------------> Starting -----------> Idle
        |                                 |               ^   |
        |<--------------------------------+               |   | an operation
        |   ReResolve or Deny, a failed BEGIN,            |   | starts
        |   or cancellation before BEGIN returns          |   v
        |                                                 |  InFlight
        |            operation completes -----------------+     |
        |                                                 |     |
        |          ROLLBACK TO SAVEPOINT -----------------+     | a statement
        |          (nested reject; the outer                    | errored
        |           transaction continues)                      v
        v                                                    Poisoned
    Cancelling <-- a forcing publisher wins the gate in         |
        |          Idle, InFlight, Quiescing or Poisoned        | settle
        |                                                       | requested
        |          settle requested while a command owns        |
        |          logical execution                            |
        |                      |                                |
        |                  Quiescing                            |
        |                      |                                |
        |                      | that command returns           |
        |                      |                                |
        |                      v                                |
        +------> Settled <--- Settling <-------------------------+
```

Settlement may be requested from `Idle`, `InFlight` or `Poisoned`. From `Idle`
and `Poisoned` it goes straight to `Settling`. From `InFlight` it goes to
`Quiescing` first and waits there for the operation to return the client, rather
than treating the empty slot as proof that terminal SQL ran.

A *force* takes the other exit. From any of the six states before terminal SQL
is issued - `Preparing`, `Starting`, `Idle`, `InFlight`, `Quiescing`,
`Poisoned` - it enters `Cancelling`, which reaches `Settled` without passing
through `Settling`, because forced cleanup is not terminal SQL the reducer
issued. `Settling` is the exception and the gate section says why.

- **Preparing** - admission is granted, the RAII claim guard is held and the
  execution deadline is armed, and the platform-role authority read that decides
  whether this transaction may proceed is in flight. **No `BEGIN` has been
  sent.** That read's verdict decides what happens next: `Current` proceeds to
  `Starting` and captures the `BEGIN` ceiling from it, while `ReResolve` and
  `Deny` end the transaction without ever issuing `BEGIN`.

  Time spent waiting for admission is *not* part of this state, and the deadline
  is armed on the transition into it. Queue time therefore does not consume the
  transaction's execution budget.
- **Starting** - the authority read returned `Current`; `BEGIN` and session
  setup are in flight, and no operation can see the session yet.
- **Idle** - transaction open, no operation owns the client.
- **InFlight** - an operation owns the client. **Settlement may arrive here**,
  and moves the transaction to `Quiescing` rather than to `Settling`.
- **Quiescing** - a settlement was requested while a command still owns logical
  execution. The intent - either a frame close or a root settle - is latched,
  and **no frame or terminal SQL is issued until the active command returns**.
  Exactly one intent is latched: a second request naming the same attempt joins
  the waiter already there and issues no second command, and a different attempt
  is a settle conflict.

  `Quiescing` is distinct from `Settling` because `Settling` *promises terminal
  SQL has already been issued*, and `Quiescing` promises it has not. Collapsing
  the two is precisely what lets an absent client look like proof that terminal
  SQL ran, which is rule 1's defect; invariant 15 is the property that keeps
  them apart.
- **Poisoned** - a statement errored. PostgreSQL refuses every further *data*
  statement until the transaction ends
  (`libs/compio-postgres/src/client.rs:507-533`), so this is a real server-side
  state, not a bookkeeping flag.

  **A `COMMIT` from `Poisoned` is reachable and must be handled, not
  forbidden.** A creator callback can swallow the error and resolve, so the
  orchestrator issues `COMMIT`; PostgreSQL accepts it and answers with the tag
  `ROLLBACK`. The transition is legal, its outcome is **failure**, and the
  machine records it as such. That is the L8 case, covered by
  `commit_that_postgres_rolled_back_must_not_report_success_l8`.

  A **savepoint** rollback does *not* leave `Poisoned` terminal: `ROLLBACK TO
  SAVEPOINT` returns the transaction to `Idle`, which the tree implements and
  tests (`crates/zeroship-plugin-db/tests/native_transaction.rs:671-705`, the
  nested-inner-reject case where the outer transaction continues and commits).
  The state diagram carries that arc; without it the machine would forbid a
  behaviour the product ships.
- **Cancelling** - the transaction is being ended by a route that is not
  terminal SQL. The single **cleanup cause** is latched, every accepted responder
  has been moved aside to be answered from the terminal outcome, and a **cleanup
  goal** is fixed from the state that was interrupted. No creator request is
  accepted here and no creator data SQL is ever issued again.

  Exactly four causes reach it, and the first is the only one that races
  anything: a **force** that won the gate, a **failed `BEGIN`**, a **`BEGIN`
  that may or may not have opened**, and **backend health the reducer itself
  discovered to be unknown**. The last three are found under the reducer lock
  with no publisher to arbitrate against, so they take this state without
  claiming the gate.

  What distinguishes it from `Settling` is who owns the session. `Settling`
  means *the reducer issued terminal SQL on a session it owns*. `Cancelling`
  means the reducer does **not** own the session - it was never opened
  (`Preparing`), it may or may not have been opened (`Starting`), or a command
  still holds it (`InFlight`, `Quiescing`) - so cleanup is delegated to the
  backend as a cancellation, and the state waits for the acknowledgement that
  says what the backend actually did. The admission claim is held until that
  acknowledgement arrives, or until the `CancellationSql` deadline expires and
  the session is withdrawn.
- **Settling** - terminal SQL has been issued and not yet answered.
- **Settled** - terminal, with a recorded outcome.

`Poisoned`, `Preparing`, `Quiescing` and `Cancelling` are why five labels were
not enough. Each names a condition a shorter list has to fake somewhere else: as
a bookkeeping flag beside the state, as an early `Starting` that has already
issued `BEGIN` on an authority nobody checked, as an empty client slot, or as a
`Settling` that never issued the SQL its own definition promises.

### Forced cleanup owns a state, and routing it through `Poisoned` is unsound

When a force wins - a deadline, a cancel, a detach, or a `ReResolve`/`Deny`
verdict - the transaction enters `Cancelling`. It does **not** pass through
`Poisoned`, and it does **not** pass through `Settling`. Three separate reasons,
each sufficient on its own, and the first is why this is a correctness decision
rather than a naming one:

1. **`Poisoned` is recoverable, and a force must not be.** Invariant 13 lets a
   successful `ROLLBACK TO` of the recovery child return the parent to `Idle`.
   So a transaction parked in `Poisoned` by an expired deadline or by a
   `Deny(AuthorityDomainMismatch)` verdict can be walked back to `Idle` by the
   creator's next `rollbackTo` and resume issuing data SQL - under an authority
   the classifier terminally denied, past a deadline that already fired. That is
   a privilege defect, not a cosmetic one, and it follows from this document's
   own invariant rather than from any observation about the artifact.
2. **`Settling` promises SQL that a force has not sent.** A force winning in
   `Preparing` issues no `BEGIN` and therefore no terminal SQL at all; a force
   winning in `Starting` does not know whether a transaction exists to end.
   Reporting either as `Settling` is exactly the collapse `Quiescing` exists to
   prevent, taken from the other direction: `Settling` would stop meaning
   "terminal SQL has been issued".
3. **A force in `InFlight` or `Quiescing` cannot issue terminal SQL.** A command
   owns the session, and invariant 5 permits one active backend token. Waiting
   for that command to return is what `Quiescing` does for a cooperative settle
   and is precisely what a force must not do - the deadline fired *because* the
   command is not returning.

`Cancelling` therefore carries a **cleanup goal**, fixed from the state the
force interrupted, and the backend's acknowledgement is read against it:

| Cleanup goal | Fixed when the force won in | The acknowledgement that proves it |
| --- | --- | --- |
| `NoTransaction` | `Preparing` - `BEGIN` was never sent | the session reports no open transaction |
| `AbortIfOpened` | `Starting`, or any failure that leaves backend health unknown | either "no open transaction" or "rolled back" |
| `OpenTransaction` | `Idle`, `InFlight`, `Quiescing`, `Poisoned` - `BEGIN` was confirmed | "rolled back" |

Both backends already expose the oracle this table reads, and it is the same one
the terminal classifier uses: PostgreSQL's `transaction_status()`, whose `None`
means *indeterminate* and is documented as such
(`libs/compio-postgres/src/client.rs:3170-3188`), and SQLite's `is_autocommit`
sample (`sc2:262-266`).

**WHEN the oracle is sampled is load-bearing, and sampling it too early
destroys healthy connections.** On PostgreSQL `transaction_status()` returns
`None` while a request is in flight, and a *failed* statement's trailing
`ReadyForQuery` is not consumed when its `await` returns. Inside a poisoned
block - where every data statement fails with `25P02` - **no retry makes the
oracle answer**. Since `None` is indeterminate and indeterminate withdraws the
session, a driver that samples on entry to `Cancelling` would withdraw a
perfectly healthy connection on *every* forced cleanup.

So the oracle is sampled **after** the cleanup `ROLLBACK`, which succeeds and
resolves the byte - never before it. This is a constraint on the driver, not on
the reducer, which is why it belongs here rather than being left to whoever
writes the driver to rediscover.

An acknowledgement that proves the goal settles the transaction with the latched
cause. **Anything else - an acknowledgement that contradicts the goal, or one
that is indeterminate - settles as indeterminate and withdraws the session**:
the connection is destroyed rather than returned, so no later user can inherit
it. That is the whole of the "unknown health" disposition; see invariant 4.

**`AbortIfOpened` absorbs the unknown-health case, and that is a decision of
this contract.** The artifact carries a fourth goal, `QuarantineUnknown`, which
exists only to route into its `Quarantining` state. Its content is "we do not
know whether a transaction is open" - which is what `AbortIfOpened` already
says. Dropping it costs nothing this contract can observe.

## The lifecycle classifier

Every authority observation runs through **one total classifier** before any
data SQL: the read `Preparing` waits on, the read each operation takes before
its own data SQL, and any unsolicited lifecycle observation a publisher submits.
It compares the observation against the authority and schema epoch the binding
captured, and returns exactly one of three verdicts.

| Verdict | Returned when | What the protocol does |
| --- | --- | --- |
| **`Current { ceiling }`** | app id, authority domain and incarnation all match, the app's lifecycle state is stable, and the observed schema epoch equals the expected one | proceed. **Not forcing**: it never claims the gate. Its ceiling is folded into the effective ceiling by `meet`, so it can only tighten |
| **`ReResolve`** | identity matches, but the lifecycle state is *changing*, or the epoch differs | **retryable.** The attempt is rolled back and the caller receives an epoch-changed error. The entry never follows the new epoch in place; the caller re-resolves to a fresh `TxKey` |
| **`Deny(reason)`** | app id, authority domain or incarnation differs, or the app is deprovisioned | **terminal.** No `BEGIN`, no data SQL, no following the new app. The caller receives the *specific* denial: `APP_DEPROVISIONED`, `STALE_APP_INCARNATION` or `AUTHORITY_DOMAIN_MISMATCH`, never a collapsed one |

**The order inside the classifier is load-bearing:** identity is compared before
lifecycle state, so an observation that names a *different* app is denied for
the identity mismatch rather than for whatever that other app's lifecycle
happens to be. That is what makes invariant 1's split - epoch mismatch
re-resolves, domain or incarnation mismatch denies terminally - a consequence of
one function rather than a second rule that can drift away from it.

### The denial reason reaches the creator, and the ordering is what makes that safe

The three denial reasons the parent design keeps distinct stay distinct all the
way to the caller. Each is a typed terminal error, not an audit row:
`APP_DEPROVISIONED`, `STALE_APP_INCARNATION`, `AUTHORITY_DOMAIN_MISMATCH`. The
parent design already places all three in a table headed **"Error contract"**
(`docs/proposals/2026-08-26-runtime-db-binding-design.md:1010-1020`), which is a
caller-facing surface; its later remark about the audit trail names a second
consumer, not the only one.

None of the three is retryable, which that same table states. **Non-retryable is
not the same as indistinguishable**, and the three differ in what the *next*
action should be:

- `APP_DEPROVISIONED` is a permanent tombstone. There is nothing to re-resolve
  to, now or later.
- `STALE_APP_INCARNATION` means the app is alive under a new incarnation. This
  handle is dead; a freshly resolved binding is not. Re-resolving is the correct
  next action, and a caller that cannot tell this from a tombstone either
  re-resolves against a deprovisioned app forever or gives up on a live one.
- `AUTHORITY_DOMAIN_MISMATCH` means the cluster or timeline answering is not the
  one the binding captured. Re-resolving locally does not help; this is an
  operational fault, not a lifecycle event.

Collapsing them would make the only distinguishable signal a log line the
creator cannot read, which turns that choice into a guess.

**What this leaks, and why it is nothing.** `Deny` is only ever returned to a
caller whose identity *matched*, because the ordering above compares identity
first: an observation naming a different app is denied for the identity mismatch
and never reaches the lifecycle arm. So `APP_DEPROVISIONED` tells an app that
its own authority record carries a tombstone. It is not an oracle over other
tenants, and it does not distinguish "another app is deprovisioned" from "no
such app" - that question is answered by the identity comparison, uniformly, for
every value it could take.

The reverse choice leaks more. Auditing the reason and returning a single
collapsed error hides a *permanent* condition behind a *retryable-looking* one,
so the observable difference migrates from an error code into retry timing -
which every caller can measure and no caller can act on correctly.

This classifier is also the comparison "Two identities, deliberately distinct"
promises when it leaves the authority domain out of the admission key: the
domain is checked here, at the authority read, against the binding's captured
value, on every observation rather than once at admission.

`ReResolve` and `Deny` are forcing publishers; `Current` is not. **The reducer
re-runs the classifier on the observation itself** rather than trusting the
verdict a publisher attached, so labelling a `Deny` as `Current` buys nothing:
the label is an input, never a verdict.

The ceiling folds as `meet(begin_ceiling, effective_ceiling,
newly_read_ceiling)`, which is invariant 8 - a raise is ignored until a new
top-level transaction, a lower value tightens the next authorization. The read
happens on the platform-role session and never borrows the tenant data session,
which is Fork B and invariant 7.

## The rules that are actually load-bearing

1. **An absent client is never proof that terminal SQL ran.** Today's settle
   path does exactly that: `take_tx_client_for` returning `None` falls into
   "Slot already drained ... Treat as settled", releases the claim, clears
   pending emits, and returns `SettleOutcome::Ok` **without sending anything**
   (`transaction/mod.rs:1034-1039`). That is DBR-03. Under this contract, a
   settle arriving while the state is `InFlight` **waits for the operation to
   return the client**; only a state of `Settled` ends a settle early.

2. **Terminal SQL inspects its command tag.** This is already true - the settle
   path judges its terminal command tag, covered by
   `commit_that_postgres_rolled_back_must_not_report_success_l8` - and this
   contract must not regress it. A `COMMIT` answered `ROLLBACK` is a failed
   transaction, not a successful one. The check is scoped to the PostgreSQL
   `COMMIT` arm deliberately: `RELEASE` answers with the tag `RELEASE`, so a
   broader "anything but COMMIT is a failure" test would reject every healthy
   nested commit.

3. **Pending effects are discarded unless the commit is confirmed** - which is
   already the behaviour, and this rule exists to keep it rather than to fix it.
   `clear_pending_emits` clears the queue "without firing any events"
   (`crates/zeroship-plugin-db/src/exec.rs:615-621`), and every settle arm other
   than `(true, Ok)` calls it; the queue is dropped, not published.

   The rule belongs here because the state machine must not *introduce* the
   defect: an effect buffer keyed per transaction, published only from a
   confirmed-commit arm, is what preserves today's behaviour once settlement
   stops being a single early-return.

4. **The deadline is enforced by an independent timer, not by the settle path.**
   A deadline enforced by the settle path is circular - a body that never
   settles never reaches the settle path. The timer is armed on the transition
   into `Preparing` - the same transition that grants admission and creates the
   claim guard - and fires regardless of callback behaviour, moving the
   transaction to `Cancelling` with the cleanup goal its interrupted state
   fixes. Its slot protocol is under "The deadline slot".

5. **Cancellation before `BEGIN` returns must not leak the admission claim.**
   That window is exactly `Preparing` and `Starting`. An RAII guard is armed at
   admission - the transition into `Preparing` - and disarmed only once the
   client is installed. Today the claim can outlive the isolate, leaving later
   transactions for that app parked indefinitely. That defect is labelled
   **DBR-11**.

**The `DBR-` numbering resolves to nothing in the tracked set.** `DBR-03`,
`DBR-04/05`, `DBR-06` and `DBR-11` occur in no other document of this set, and
these defects carry no `L` number in the register. The labels are kept because
they are the only handle the defects have, but the numbering is **owed** either
a definition or register rows.

## The deadline slot

Rule 4's timer is not a bare sleep, because the deadline it enforces is
replaced, not merely cancelled, as the transaction moves. The transaction owns
**one** deadline slot, shared by every deadline this protocol arms, holding
exactly one of three values:

```text
Disarmed
Armed  { kind, generation, at }
Fired  { kind, generation }
```

A timer task carries only the transaction key, the `kind`, the `generation` and
the event sender. It owns no session, no client and no settle future, which is
what makes rule 4's "independent of callback behaviour" true rather than
aspirational.

A `generation` is minted fresh at every arming and is **never reused across
kinds**, so the number alone never authenticates a fire - the `(kind,
generation)` pair does. The slot has exactly four mutators and no others:

| Mutator | Legal from | Effect |
| --- | --- | --- |
| `arm_initial(kind, generation, at)` | `Disarmed` only | becomes `Armed`, and schedules the timer |
| `claim_fire(kind, generation)` | `Armed` on **that exact pair** | flips to `Fired` atomically, granting the caller the sole right to act on that expiry |
| `replace_current(expected_kind, next_kind, next_generation, next_at)` | `Armed` **or** `Fired`, of `expected_kind` | becomes `Armed` on the successor kind with a fresh generation, and schedules it |
| `disarm` | any state, naming the terminal generation | becomes `Disarmed`, at terminal cleanup |

**Every other call is a pure diagnostic.** It returns
`StaleTransactionDeadline` and produces no SQL, no reply, no claim change, no
interrupt and no state mutation. A duplicate delivery of an already-claimed
timer is exactly that case, which is why `Armed -> Fired` has to be one atomic
flip rather than a check followed by a mutation: a check-then-mutate lets two
deliveries of the same expiry both believe they own it, and guard order step 3
would then be ordering a claim that does not exclude.

`replace_current` accepts `Fired` as well as `Armed` deliberately. A deadline
that has already fired and driven the transaction into forced cleanup must
still be replaceable by the deadline that bounds *that* cleanup; otherwise a
hung rollback is unbounded and strands the admission claim, which is rule 5's
DBR-11 failure reached by a second route.

### The kinds are closed at three, and that is what closes invariant 3

Every state that can outlive a caller is bounded by exactly one kind, and each
kind is reached by replacing the one before it:

| Kind | Armed on entry to | Bounds | Replaces |
| --- | --- | --- | --- |
| `Execution` | `Preparing` | everything a caller can see: admission-to-terminal | - (`arm_initial`) |
| `CancellationSql` | `Cancelling` | forced cleanup, from the force winning the gate to the acknowledgement | `Execution` |
| `TerminalSql` | `Settling` | terminal SQL, from issue to answer | `Execution` |

There is no fourth. `Cancelling` and `Settling` are the only states that wait on
a backend that owes an answer and has no caller left to give up, and each has
its bound.

**Every call site therefore names `Execution` as its expected kind**, so
`replace_current`'s `expected_kind` is not decoration: a second force arriving
in `Cancelling` finds `CancellationSql` current, fails the expectation, and is a
pure diagnostic rather than a second cleanup with a fresh generation.

**What happens when the second-stage deadline fires is the answer this contract
owes.** `CancellationSql` or `TerminalSql` expiring means the backend did not
answer within its grace. There is no third timer and no escalation: the session
is **withdrawn** - the physical connection destroyed rather than returned - and
the transaction settles as indeterminate, carrying the latched cause. Destroying
the connection is the fence, and it is sufficient here because the admission key
is process-local and protects the *transaction connection slot*, not the
server-side transaction: once the connection is gone nothing can reach that
session, so the claim can be released and the slot reused.

That closes **invariant 3**, which the four-mutator table alone did not: every
fired deadline now has a bounded path to `Settled` that requires no cooperation
from the backend.

**Accepted cost, stated rather than hidden.** Closing the socket does not
guarantee the server-side transaction is *already* gone - a PostgreSQL backend
mid-statement need not notice the disconnect promptly - so a following
transaction for the same app can still block on locks that transaction holds.
The cancellation request sent on entry to `Cancelling` is what bounds that in
practice. The **safety** property this contract asserts - no later SQL on that
transaction, and no reuse of a session in unknown state - holds unconditionally;
the liveness of lock release does not, and belongs to `statement_timeout`.

### Why there is no fifth mutator

The artifact carries `replace_with_retirement`, and its invariant 30 lists it as
one of five. **That fifth mutator has exactly one caller**,
`begin_generation_retirement` (`dbbind-r7-codex.md:2046-2079`), whose only
product is the `Quarantining` state; the single kind it arms, `RetirementFence`,
is consumed by no other state. Adopting it means adopting `Quarantining`, which
means adopting the supervisor, the signed `GenerationRetirementProof` and the
durable fence jobs this contract declines. The two questions are one question,
and they are answered together under invariant 4.

**The artifact's own "five" is not a closure claim to inherit either.** Its
invariant 30 omits `rearm_retirement`, which its `Quarantining` retry row calls
(`:2584`), and `replace_with_cancellation_sql_infallible`, which its
`Cancelling` constructor calls (`:2374`). The source implements at least seven
and states five. So "four" here is not a subset promoted to a closure claim
against a source that said five - it is a closure over the three kinds named
above, which is a property an implementer can check by counting arming sites.

## Frames and effects

The transaction owns a **strict-LIFO frame stack**. Only the innermost open
frame may issue data SQL, open a child, or close. The root frame is created
only once `BEGIN` is confirmed; a child is inserted as `Opening` before
`SAVEPOINT` is sent and becomes `Open` only when that command succeeds.

**Effects are per-frame, not per-app** - a flat app-keyed buffer lets a
top-level `COMMIT` drain a rolled-back child's queue and tell a subscriber
about a row that does not exist - and their fate is a property of how the frame
closed:

| Frame closes by | The frame's queued effects |
| --- | --- |
| `RELEASE` succeeds | appended to the parent, in order. Nothing is published |
| `ROLLBACK TO` succeeds | **discarded** - the database changes they describe are known to be undone |
| `ROLLBACK TO` fails | retained for diagnosis; the transaction poisons, ends, or withdraws by health (invariant 15); nothing is published |
| `ROLLBACK TO` succeeds but the following `RELEASE` fails | effects **stay discarded**; an empty child frame remains; root cleanup is forced |
| confirmed **root** commit | detached and published |
| every other terminal outcome | every frame buffer discarded |

The release-after-rollback row decides **whether the frame still exists**. A
rolled-back frame is not closed until its `RELEASE` lands, so until then the
transaction is not `Idle` and the frame is not available for new work - a point
the state table must agree with rather than reporting `Idle` the moment
`ROLLBACK TO` returns.

**Ordering is contract, not implementation detail.** The frame's fate is applied
**after** the statement succeeds, never before. Discarding on the assumption
that `ROLLBACK TO` will succeed makes the failure row above unachievable - the
diagnostic evidence it calls for is already gone by the time the failure is
known. The settle path takes the frame's watermark, runs the statement, and only
then applies the fate.

What ships today is the minimal form of this table: a watermark per frame into
one shared buffer. The per-frame `Vec` above is the same contract with a cleaner
representation, and an implementer may keep either as long as the fates match.

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

## Where the full transition matrix lives

This document states the *contract*: the states, the load-bearing rules, the
guard order, the gate protocol and the invariants. It deliberately does **not**
inline the exhaustive legal-transition table and illegal-transition matrix -
several hundred rows of `(state, event) -> action, next state, typed error`.
That belongs with the implementation, next to the `match` arms it becomes, not
in a contract someone has to read end to end.

The exhaustive version exists and was produced against this design: the closed
state enum, the closed event and completion enums, the complete legal transition
table, the exhaustive illegal matrix with a typed error per cell, the full
deadline-kind set, and the terminal outcome table. It is preserved at
`docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md` (untracked, beside these
drafts). An implementer should work from that for the matrix; a reviewer should
work from this.

**Its state enum has twelve members to this one's nine, and every difference is
accounted for.** It also carries `WaitingAdmission` for the pre-admission queue,
and `HardStopping` and `Quarantining` for the generation-fencing machinery.

- `WaitingAdmission` is deliberately absent. Queue time is not part of the
  execution budget and no deadline is armed before admission, so a waiting
  caller has no state a guard can be keyed to. The queue is a property of the
  admission key, not of a transaction that does not exist yet.
- `Cancelling` **is adopted**, in the reduced form under "States": a cause, a
  cleanup goal and an outstanding acknowledgement. What is dropped from it is
  the artifact's `CancelCleanupPhase::HardStopping` arm - the permit, the
  trigger and the fence token - along with the `QuarantineUnknown` goal that
  only routes into `Quarantining`.
- `HardStopping` and `Quarantining` are declined, and "The kinds are closed at
  three" says what replaces them: withdrawing the session, which needs no
  supervisor because it is the connection itself that is destroyed.

**It is not SC-1 written out in full, and the difference is a scope trap.** That
artifact invokes a *supervisor*, a `FenceJobRegistry` and durable fence jobs
with a recovery scan, so that actor or publisher death is irrelevant to terminal
delivery. **This contract does not adopt any of it** - see "Terminal delivery
does not survive process death". `FenceJobRegistry` occurs nowhere in the
codebase; do not build one because the artifact names it. Treating that
machinery as part of the step is what turns "write the reducer" into "write the
reducer, plus a durable job system".

Where the two disagree elsewhere, **this document is not automatically right** -
it is the more readable one, which is a different property. The invariants below
are the arbiter, because they are the part a property test can actually check.

## Guard order is normative, not an implementation detail

Which guard runs first decides **which error the caller sees**, and two orders
that accept and reject exactly the same event sequences can still report
different reasons for the same rejection. That makes the order part of the
contract. It runs:

1. **Identity.** Authority domain and incarnation, checked against the routed
   key. A mismatch returns `AppIncarnationMismatch` and **must not touch that
   entry's session, actor, timer or admission** - not even to cancel it. A
   detach request additionally requires the expected authority to equal the
   stored one, and a mismatch there interrupts nothing.
2. **Token match.** Every completion must carry the token its current action
   minted; a wrong one is `StaleTransactionCompletion` (or
   `StaleAdmissionCompletion`) and changes nothing. Cancellation completions
   match the entry's single cancellation token, not the command token.
3. **Deadline preflight, before claiming the timer.** A fired deadline runs a
   **pure** state/event capability check under the reducer lock first, and only
   a preflight-legal event is allowed to claim the fire. The claim succeeds only
   if the shared slot is `Armed` with that exact (kind, generation) pair, and
   flips it to `Fired` atomically. A wrong pair is a pure diagnostic -
   `StaleTransactionDeadline`, no SQL, no reply, no state change. Ordering
   it this way is what stops an illegal cell from claiming a timer or queueing a
   forced settlement as a side effect of being rejected.
4. **Backend generation.** A completion naming a generation other than the one
   the admitted handle stored is stale and touches nothing.
5. **The state/event matrix.** Its not-ready / busy / expired error wins
   whenever the state cannot legally process the event at all.
6. **Only then** the frame guards - top, root, depth - and only inside a state
   where the matrix already marked that event potentially legal. None of them
   mutates the stack: a non-top parent is `SavepointNotCurrent`, a ninth
   simultaneous child is `SavepointDepthExceeded`, closing root is
   `SavepointRootCannotClose`.

Two consequences that are contract rather than style:

- **`OpenFrame` takes no caller-supplied child id.** The registry mints a
  never-reused frame id and savepoint name from its own sequence. A
  caller-supplied id would let a creator collide two frames or resurrect a
  closed name, and invariant 9's "never repeats" could not then be enforced.
- **Every illegal matrix cell has a type-correct error path.** Reply channels
  carry `Result<_, TxProtocolError>`, so a rejection is a value the caller
  receives - never an out-of-band log line. A protocol whose illegal transitions
  are only observable in a worker log is not executable.

## One gate for every forcing publisher, not just caller cancellation

The deadline is only safe if it cannot race an ordinary completion, and the
naive design guards the wrong set: it mediates **caller cancellation** and
leaves the deadline, detach and lifecycle-denial paths to race. All four can
force a transaction to end, so all four go through **one** gate.

The forcing publishers are: explicit `Cancel`, caller-drop cancel, the execution
deadline firing, an exact `DetachRequested`, and a lifecycle observation the
classifier has already ruled `ReResolve` or `Deny`. A lifecycle observation
ruled `Current` is **not** forcing and does not claim the gate - and the reducer
re-runs the classifier itself, so a publisher cannot smuggle a `Deny` through by
labelling it `Current`.

The gate has three outcomes, and the middle one is the one a hand-rolled
implementation gets wrong:

- **Force won.** The gate was open; the force latches the single cleanup cause
  and the ordinary completion is suppressed.
- **Completion won.** An ordinary completion was already queued. The force is
  enqueued *after* it rather than replacing it, and the reducer processes the
  ordinary result **first**, then enters `Cancelling` from the state that
  results - with the cleanup goal that state fixes, which is why the goal is
  read at entry and not at the moment the force was published. It does not
  discard a completion that already happened, and it does not pretend the force
  arrived first.
- **Joined.** An earlier force already owns the cause. The second one joins it
  and emits nothing. **Exactly one cleanup cause is ever latched**, so the
  reason a transaction ended is deterministic rather than last-writer-wins -
  which matters because that cause is what the creator is told.

Two rules make this airtight and both are load-bearing:

1. **Lock order is fixed: command gate, then terminal owner. The reverse is
   forbidden.** That ordering is the whole reason a completed terminal owner can
   never coexist with a still-open command gate that a deadline could then flip
   to forced. Without it the two guards are individually correct and jointly
   useless.
2. **Once a terminal completion is promised, a later forcing publisher neither
   sets a new force nor suppresses the promised result** - it is treated as a
   late cancel awaiting `AlreadyCompleted`. A deadline that fires microseconds
   after a commit succeeded must not turn that commit into a cancellation.

This applies in `Preparing`, `Starting`, `Idle`, `InFlight`, `Quiescing` and
`Poisoned` alike - every state from which `Cancelling` is reachable. Scoping it
to one state is the same mistake as scoping it to one publisher. `Idle` is not
an afterthought in that list: rule 4's central case, a callback that never
settles, fires against a transaction sitting in exactly that state.

**`Settling` is the one nonterminal state a force cannot claim**, and rule 2
above is why. Terminal SQL has been issued exactly once and the backend owes an
answer; a force arriving now would be starting a competing cleanup on a session
that is already ending. It is a late cancel: it joins the terminal waiters and
changes nothing. The `TerminalSql` deadline, not a force, is what bounds that
wait, which is also why `replace_current` never has to accept `TerminalSql` as
an expected kind.

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
   once no live transaction or reservation remains. This holds unconditionally
   only because every state that waits on a backend is bounded and every bound
   ends in withdrawing the session rather than in waiting longer; see "The kinds
   are closed at three".
4. **Session conservation.** Between confirmed `BEGIN` and terminal cleanup,
   session ownership is exactly one of the registry, the matching command token,
   or **withdrawn** - never silently absent.

   **`Withdrawn` is a session-ownership value, not a transaction state**, and it
   is the whole of this contract's answer to unknown backend health: the session
   is never returned to the registry, never leased to another command, and its
   physical connection is destroyed at terminal cleanup rather than reused. It
   requires no supervisor, no signed retirement proof and no `Quarantining`
   state, because closing the connection *is* the proof that nothing further can
   run on it. SC-2 spells the same disposition as a verb over its actor's
   connection (`sc2:269`, `sc2:275`), and the failure it prevents is the one the
   driver already names: handing the next user of a connection an aborted
   transaction (`libs/compio-postgres/src/client.rs:3180-3184`).
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
    discards exactly the child sequence **and no parent effect**. When a frame's
    watermark is missing the buffer is left alone rather than truncated to zero,
    because truncating would discard parent effects: over-publishing is a bug,
    and silently dropping a committed row's event is a worse one.
12. **Commit-only publication.** Nothing publishes unless the root **intent was
    commit** AND the root finish result was committed, and a confirmed commit
    publishes every retained effect exactly once, in order. A `Committed`
    result returned against a *rollback* intent is a terminal-result
    **mismatch**, not a commit: it publishes nothing. (The intent half is easy
    to omit, and omitting it means a backend that answers the wrong terminal
    verb gets to publish.)
13. **Poison rule.** No data, frame-open or release command may start from
    `Poisoned`. A successful rollback-to or release of the recovery child
    returns the parent to `Idle`; otherwise **only root settlement** can end it.
    `Poisoned` is therefore *recoverable by construction*, which is exactly why
    no forced cleanup may route through it - see invariant 16.
14. **Poisoned commit.** A `COMMIT` answered `ROLLBACK` never resolves a
    creator promise and never publishes an effect. This one is not speculative -
    it is defect **L8**, now closed
    (`docs/proposals/2026-08-26-runtime-db-binding-defects-closed.md`), and its
    reachable case is already covered by a live regression test
    (`crates/zeroship-plugin-db/tests/native_transaction.rs:977`,
    "L8 REVEAL: a transaction PostgreSQL rolled back must not be reported as a
    successful commit"). It is listed here so the property test inherits the
    property rather than the single case. **That citation only counts under a
    specific invocation**; see "The invocation these arms require" under
    Acceptance shape.
15. **Quiescing.** Settlement arriving while another command owns logical
    execution issues **zero** frame or terminal SQL until that command returns;
    it then starts at most one logical settlement attempt, and never more than
    one SQL statement concurrently. A successfully closed rollback-to frame
    settles as its ordered `ROLLBACK TO` then `RELEASE` pair; if the
    `ROLLBACK TO` fails, the legal error row stops **without** `RELEASE` and
    retains the frame's effects, then takes exactly one of **three**
    dispositions, chosen by the same health oracle the cleanup goals read
    (`transaction_status()` on PostgreSQL, `is_autocommit` on SQLite):

    | The oracle says | Disposition |
    | --- | --- |
    | a transaction is still open and in error | **poisons**; invariant 13 governs it from there |
    | no transaction remains - the backend ended it | **terminal**. Settle as aborted-by-backend, discard every frame buffer, release the claim. No `Cancelling`: there is nothing left to cancel |
    | it cannot say | `Cancelling` with goal `AbortIfOpened`, and the session is **withdrawn** per invariant 4 |

    There is no fourth. The three exhaust the oracle's range, which is what
    makes this checkable rather than a list of remembered cases.
16. **Forced cleanup is terminal.** Once `Cancelling` is entered, no path leads
    back to any state that can issue creator data SQL, and exactly one cleanup
    cause is ever latched. Every exit is `Settled`. A property test asserts this
    on prefixes that interleave a force with a creator `rollbackTo`, which is
    the shape that would otherwise resurrect a denied or expired transaction
    through invariant 13's recovery arc.

Invariants 13-16 are the ones that decide whether the state table is executable
or merely descriptive. 1-12 constrain what a transition may do; these constrain
what may happen when two things arrive at once - a settlement racing a command,
a commit racing a poison, a force racing a recovery - which is exactly where a
hand-written implementation diverges from its own table.

## Acceptance shape

### The invocation these arms require

Every citation of a `native_transaction` arm in this document - invariant 14's
included - **only counts under a specific invocation**, which is exactly the
trap the fourth defect class describes ("A fourth: the arm that was never
built", in
`docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`).
`native_transaction` declares `required-features = ["test-helpers"]`, so a
plain `cargo test -p zeroship-plugin-db` filters the whole target out and never
builds it. The command that runs it is

    cargo test -p zeroship-plugin-db --test native_transaction \
      --features test-helpers -- --test-threads=1

Five of this crate's test targets are gated that way. Citing an arm without
naming its invocation is how "already covered" comes to mean "compiled by
nobody's routine command".

### The arms

A state table plus one test per illegal transition, and explicitly:

- **a FAILED `ROLLBACK TO` retains the frame's effects**, asserted by faulting
  the rollback statement after an effect is queued: the child effects and the
  frame remain diagnostically present, nothing publishes, the transaction takes
  invariant 15's disposition for the health the fault produces - poison it with
  an ordinary statement error - and root cleanup finally discards them.

  This arm is required because the success-path arm below **cannot fail against
  a premature discard** - a successful-rollback regression test stays green
  while the failure path destroys the evidence this contract requires it to
  keep. A table with two rows needs an arm for each row; asserting only the row
  that already passes measures nothing about the other.
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

  The black-box form of it is **already written and already green**: the probe
  exists (`examples/db-todos/src/index.ts:569-599` starts the pair in one
  `Promise.all`) and the dev-vs-deployed gate asserts all three halves -
  `want cxPar 'two transactions in one Promise.all: leg 1 commits'`,
  `... leg 2 commits`, and `... both concurrent transactions left their row`
  (`tests/e2e_dev_vs_deployed_db.sh:1215-1217`). Keep those as a **preservation
  property**. They cannot be acceptance for the work this document proposes:
  they would stay green if none of it were done.

  So the arm needs a **discriminating partner**, named concretely rather than
  gestured at. Two forms qualify:

  1. **Inspect the new state.** Assert on the `TxKey`/state reducer directly -
     that admission is keyed by the identity this document defines, and that a
     second same-key `begin` is refused or parked BY THE REDUCER. This is a
     white-box arm on purpose; the black-box shape provably cannot see it.
  2. **Mutation-delete the reducer's admission guard and prove the test goes
     red.** An arm that survives deletion of the thing it exists to check is
     not evidence, and this document should not accept one.

  The PostgreSQL cross-isolate non-contention arm is discriminating as written.
  The bare "waits and succeeds" arm is not, and no amount of strengthening its
  assertions about durability will make it so - durability is the half that was
  never in question.
- two same-app transactions in **different isolates** cannot reach each other's
  client. This half is backend-neutral.
- **PostgreSQL arm only:** two same-app transactions in different isolates do
  not contend. **On SQLite they deliberately do**, and the label is the fix.
  Why - the rejected bounded-connection alternative and the
  `docs/reference/db.md:992-997` disclaimer it rests on - is under "SQLite
  admission serializes, and bounded connections are rejected" above.
- a settle arriving while an operation owns the client waits, and terminal SQL
  is sent exactly once;
- a `COMMIT` answered with a `ROLLBACK` tag surfaces as a failed transaction and
  publishes no change events;
- a callback that never settles is terminated by the deadline, with the
  transaction rolled back and the claim released;
- cancellation between admission and `BEGIN` releases the claim, proved by a
  following transaction for the same app succeeding;
- **a forced transaction cannot be resurrected by `rollbackTo`.** Drive a force
  - the execution deadline is the cheapest - while an open child frame exists,
  then have the creator callback issue `rollbackTo` on that child. It must
  receive the latched cleanup cause, and the transaction must not become `Idle`
  or accept any later data SQL.

  This arm is the discriminating one for `Cancelling`, and it is the only arm
  here that fails against the shape this document previously described. Routing
  forced cleanup through `Poisoned` passes every other arm on this list and
  fails only this one, because `Poisoned` is the single nonterminal state
  invariant 13 lets a creator command walk back to `Idle`. Assert the cause the
  caller receives, not merely that the transaction eventually ended: a rollback
  that ends the transaction for the *wrong* reason is what the single-latched
  cause exists to prevent.
- **a second-stage deadline settles rather than hanging.** Fault the backend so
  the forced rollback issued from `Cancelling` never answers. The
  `CancellationSql` deadline must fire, the session must be withdrawn rather
  than returned, the transaction must reach `Settled` with an indeterminate
  outcome, and a following transaction for the same app must be admitted. This
  is invariant 3's claim balance on the path that previously had nothing to
  release the claim, so an implementation without the second-stage deadline
  hangs this arm instead of failing it - give it a bound and treat a timeout as
  a failure, not as an inconclusive run.
