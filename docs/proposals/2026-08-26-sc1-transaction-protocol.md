# SC-1: the explicit transaction protocol

**Status.** PARTIAL. The state machine ships and drives every production
`db.transaction()`: `crates/zeroship-data-orm/src/transaction/` holds the pure
reducer (`reducer/`, with `identity.rs`, `deadline.rs`, `frames.rs`), the driver
that turns an `Action` into I/O (`driver.rs`), the orchestrator
(`mod.rs`) and the test seam (`probe.rs`). Two inputs the design assumes are
missing; see Open 1 and Open 2.

## What it is

### Two identities, deliberately distinct

| Identity | Key | The question it answers |
| --- | --- | --- |
| **Registry identity** | one entry per live transaction | Which transaction is this? |
| **Admission identity** | PostgreSQL: `(runtime instance, app, incarnation)`. SQLite: `(thread resource, app, incarnation)` | May this transaction *start*? |

**Registry identity** indexes the state entry, the savepoint frames and the
pending-effect buffer. It is unique per transaction, so it never contends, and
it needs no incarnation or domain: it never outlives the app instance that
created it, so it cannot alias across incarnations the way a durable key can.
In the tree the entry is a `TxLane` in a per-thread `HashMap<app_id, TxLane>`
(`crates/zeroship-data-orm/src/tx_lanes.rs`), holding the reducer, the session,
the canceller, the emit marks and the claim waiters; there is no separate
`TxKey` type.

**Admission identity** decides whether a transaction may *start*; a second one
holding the same key waits (`TxAdmission::acquire` / `AwaitTxClaim` in
`transaction/mod.rs`). That is what preserves the `Promise.all` semantics under
"Why it is this way". **Admission must not be keyed by transaction id.** Unique
ids never contend, so keying admission by them removes the serialisation without
anyone deciding to; that is why the two identities are separate keys.

The admission key differs by backend. SQLite serialises across isolates sharing
one actor, because SC-2 gives an attached app file exactly one transaction
connection. **The authority domain is not a key component**, and the
domain-mismatch denial is not weakened by that: admission is process-local and
short-lived, so it cannot outlive a timeline change the way a durable row can.
The domain is compared where it can differ, at the authority read, against the
binding's captured value.

The tree's key is coarser than the specification on both tiers: the lane map is
a `thread_local!`, so admission is `(worker OS thread, app id)` and the
incarnation carried in `AuthorityIdentity` is always `0` (Open 1). One OS thread
multiplexes many isolates, so two isolates of the same app on one thread share a
lane and serialise; two threads never see each other's lanes.

### The two tiers differ in what contention looks like

Admission serialises on **both** tiers: a second same-app top-level
`db.transaction()` waits on the claim. Below admission the backends' exhaustion
policies differ, and the difference is creator-visible on any path that reaches a
backend without the claim.

- SQLite admits one transaction connection per `(session, app)` and **refuses the
  second immediately** with `transaction_connection_busy`
  (`crates/zeroship-data-orm/src/backend/sqlite/session.rs`, `reserve_transaction`, whose own
  heading is "Exhaustion: refuse immediately, do not queue"). A refusal is decided
  from state the caller can see and needs no deadline to be safe.
- Postgres queues on the pool's `acquire_timeout`.

The divergence is stated for creators in `docs/reference/sqlite-divergences.md`;
keep that row and this section in step.

It also decides the shape of any acceptance arm about contention. **Non-contention
between two same-app transactions in different isolates is a PostgreSQL-only
arm**: on SQLite an attached app file has exactly one transaction connection, so
two admitted transactions have one connection between them. Stating that arm
backend-neutrally makes it unpassable on SQLite under any implementation. Today
it is additionally cross-thread-only, per Open 4.

### States

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

- **Preparing** - admission is granted, the RAII claim guard is held, the
  execution deadline is armed, and the platform-role authority read is in
  flight. **No `BEGIN` has been sent.** `Current` proceeds to `Starting` and
  captures the `BEGIN` ceiling; `ReResolve` and `Deny` end the transaction
  without issuing `BEGIN`. Time spent waiting for admission is *not* part of
  this state, and the deadline is armed on the transition into it, so queue time
  does not consume the execution budget.
- **Starting** - `BEGIN` and session setup are in flight; no operation can see
  the session yet. A session-setup failure is classified rather than collapsed:
  it arrives as a typed `BeginOutcome` and maps onto `Verdict::ReResolve`, a
  specific `Deny`, or forced cleanup.
- **Idle** - transaction open, no operation owns the client.
- **InFlight** - an operation owns the client. **Settlement may arrive here**,
  and moves the transaction to `Quiescing` rather than to `Settling`.
- **Quiescing** - a settlement was requested while a command still owns logical
  execution. The intent (a frame close or a root settle) is latched, and **no
  frame or terminal SQL is issued until the active command returns**. Exactly one
  intent is latched: a second request naming the same attempt joins the waiter
  already there, and a different attempt is a settle conflict.
- **Poisoned** - a statement errored. PostgreSQL refuses every further *data*
  statement until the transaction ends (`TransactionStatus::Failed` in
  `libs/compio-postgres/src/client.rs`), so this is a real server-side state, not
  a bookkeeping flag. A `COMMIT` from here is **reachable and handled, not
  forbidden**: a creator callback can swallow the error and resolve, PostgreSQL
  accepts the `COMMIT` and answers with the tag `ROLLBACK`, and the machine
  records the outcome as failure. A **savepoint** rollback does not leave
  `Poisoned` terminal: `ROLLBACK TO SAVEPOINT` returns the transaction to `Idle`.
- **Cancelling** - the transaction is being ended by a route that is not terminal
  SQL. The single **cleanup cause** is latched, every accepted responder is moved
  aside to be answered from the terminal outcome, and a **cleanup goal** is fixed
  from the state that was interrupted. No creator request is accepted here and no
  creator data SQL is ever issued again. Exactly four causes reach it, and only
  the first races anything: a **force** that won the gate, a **failed `BEGIN`**,
  a **`BEGIN` that may or may not have opened**, and **backend health the reducer
  itself discovered to be unknown**. The last three are found under the reducer
  lock with no publisher to arbitrate against, so they take this state without
  claiming the gate.
- **Settling** - terminal SQL has been issued and not yet answered.
- **Settled** - terminal, with a recorded outcome.

Settlement may be requested from `Idle`, `InFlight` or `Poisoned`. From `Idle`
and `Poisoned` it goes straight to `Settling`. From `InFlight` it goes to
`Quiescing` first and waits for the operation to return the client, rather than
treating the empty slot as proof that terminal SQL ran.

A *force* takes the other exit. From any of the six states before terminal SQL is
issued - `Preparing`, `Starting`, `Idle`, `InFlight`, `Quiescing`, `Poisoned` -
it enters `Cancelling`, which reaches `Settled` without passing through
`Settling`, because forced cleanup is not terminal SQL the reducer issued.

`Cancelling` differs from `Settling` in who owns the session. `Settling` means
*the reducer issued terminal SQL on a session it owns*. `Cancelling` means the
reducer does **not** own the session: it was never opened (`Preparing`), it may
or may not have been opened (`Starting`), or a command still holds it
(`InFlight`, `Quiescing`). Cleanup is therefore delegated to the backend, and the
state waits for the acknowledgement that says what the backend actually did. The
admission claim is held until that acknowledgement arrives, or until the
`CancellationSql` deadline expires and the session is withdrawn.

### Forced cleanup and the cleanup goal

`Cancelling` carries a **cleanup goal**, fixed from the state the force
interrupted, and the backend's acknowledgement is read against it:

| Cleanup goal | Fixed when the force won in | The acknowledgement that proves it |
| --- | --- | --- |
| `NoTransaction` | `Preparing` - `BEGIN` was never sent | the session reports no open transaction |
| `AbortIfOpened` | `Starting`, or any failure that leaves backend health unknown | either "no open transaction" or "rolled back" |
| `OpenTransaction` | `Idle`, `InFlight`, `Quiescing`, `Poisoned` - `BEGIN` was confirmed | "rolled back" |

Both backends expose the oracle this table reads, and it is the one the terminal
classifier uses: PostgreSQL's `transaction_status()`, whose `None` means
*indeterminate* and is documented as such (`libs/compio-postgres/src/client.rs`),
and SQLite's `is_autocommit` sample.

An acknowledgement that proves the goal settles the transaction with the latched
cause. **Anything else - an acknowledgement that contradicts the goal, or one
that is indeterminate - settles as indeterminate and withdraws the session**: the
connection is destroyed rather than returned, so no later user can inherit it.
That is the whole of the unknown-health disposition; see invariant 4.

Forced cleanup first **cancels**, and withdraws only when it cannot prove a
rollback. The canceller is captured at session install, so cancellation needs no
access to the session: PostgreSQL's `CancelRequest` travels on a second
connection naming the backend by process id, and SQLite's is a message to the
session actor (`driver::cancel_and_reclaim`, `backend/cancel.rs`,
`SqliteCancelHandle`). Withdrawal remains the fallback for every route that
leaves cleanup unproved.

### The lifecycle classifier

Every authority observation runs through **one total classifier** before any data
SQL: the read `Preparing` waits on, the read each operation takes before its own
data SQL, and any unsolicited lifecycle observation a publisher submits
(`reducer/identity.rs`). It compares the observation against the authority and
schema epoch the binding captured and returns exactly one of three verdicts.

| Verdict | Returned when | What the protocol does |
| --- | --- | --- |
| **`Current { ceiling }`** | app id, authority domain and incarnation all match, the lifecycle state is stable, and the observed schema epoch equals the expected one | proceed. **Not forcing**: it never claims the gate. Its ceiling is folded into the effective ceiling by `meet`, so it can only tighten |
| **`ReResolve`** | identity matches, but the lifecycle state is *changing*, or the epoch differs | **retryable.** The attempt is rolled back and the caller receives an epoch-changed error. The entry never follows the new epoch in place; the caller re-resolves |
| **`Deny(reason)`** | app id, authority domain or incarnation differs, or the app is deprovisioned | **terminal.** No `BEGIN`, no data SQL, no following the new app. The caller receives the *specific* denial: `APP_DEPROVISIONED`, `STALE_APP_INCARNATION` or `AUTHORITY_DOMAIN_MISMATCH`, never a collapsed one |

**The order inside the classifier is load-bearing:** identity is compared before
lifecycle state, so an observation naming a *different* app is denied for the
identity mismatch rather than for whatever that other app's lifecycle happens to
be. Invariant 1's split - epoch mismatch re-resolves, domain or incarnation
mismatch denies terminally - is a consequence of that one function rather than a
second rule that can drift away from it.

The three denial codes are creator-facing wire codes
(`crates/zeroship-data-orm/src/error.rs`), not audit rows. None is retryable,
but **non-retryable is not the same as indistinguishable**: `APP_DEPROVISIONED`
is a permanent tombstone with nothing to re-resolve to;
`STALE_APP_INCARNATION` means the app is alive under a new incarnation and
re-resolving is the correct next action; `AUTHORITY_DOMAIN_MISMATCH` means the
cluster or timeline answering is not the one the binding captured, which is an
operational fault that re-resolving locally cannot fix.

`ReResolve` and `Deny` are forcing publishers; `Current` is not. **The reducer
re-runs the classifier on the observation itself** rather than trusting a verdict
a publisher attached, so labelling a `Deny` as `Current` buys nothing: the label
is an input, never a verdict.

The ceiling folds as `meet(begin_ceiling, effective_ceiling, newly_read_ceiling)`
(invariant 8): a raise is ignored until a new top-level transaction, a lower
value tightens the next authorization. The read happens on the platform-role
session and never borrows the tenant data session (invariant 7).

### The load-bearing rules

1. **An absent client is never proof that terminal SQL ran.** A settle arriving
   while the state is `InFlight` **waits for the operation to return the
   client**; only a state of `Settled` ends a settle early.
2. **Terminal SQL inspects its command tag.** A `COMMIT` answered `ROLLBACK` is a
   failed transaction. The check is scoped to the PostgreSQL `COMMIT` arm
   deliberately: `RELEASE` answers with the tag `RELEASE`, so a broader "anything
   but COMMIT is a failure" test would reject every healthy nested commit.
3. **Pending effects are discarded unless the commit is confirmed.** The effect
   buffer is keyed per transaction and published only from a confirmed-commit
   arm; every other settle arm clears the queue without firing
   (`exec::clear_pending_emits`).
4. **The deadline is enforced by an independent timer, not by the settle path.** A
   deadline enforced by the settle path is circular: a body that never settles
   never reaches the settle path. The timer is armed on the transition into
   `Preparing` - the same transition that grants admission and creates the claim
   guard - and fires regardless of callback behaviour, moving the transaction to
   `Cancelling` with the cleanup goal its interrupted state fixes.
5. **Cancellation before `BEGIN` returns must not leak the admission claim.** That
   window is exactly `Preparing` and `Starting`. An RAII guard (`TxAdmission`) is
   armed at admission and disarmed only when the reducer takes ownership; from
   `Idle` onward the reducer emits `ReleaseAdmission` on *every* path to
   `Settled`, including the forced ones.

### The deadline slot

The timer is not a bare sleep, because the deadline is replaced, not merely
cancelled, as the transaction moves. The transaction owns **one** deadline slot
(`reducer/deadline.rs`) holding exactly one of three values:

```text
Disarmed
Armed  { kind, generation, at }
Fired  { kind, generation }
```

A timer task carries only the transaction key, the `kind`, the `generation` and
the event sender. It owns no session, no client and no settle future, which is
what makes rule 4's independence from callback behaviour true rather than
aspirational.

A `generation` is minted fresh at every arming and is **never reused across
kinds**, so the number alone never authenticates a fire; the `(kind, generation)`
pair does. The slot has exactly four mutators and no others:

| Mutator | Legal from | Effect |
| --- | --- | --- |
| `arm_initial(kind, generation, at)` | `Disarmed` only | becomes `Armed`, and schedules the timer |
| `claim_fire(kind, generation)` | `Armed` on **that exact pair** | flips to `Fired` atomically, granting the caller the sole right to act on that expiry |
| `replace_current(expected_kind, next_kind, next_generation, next_at)` | `Armed` **or** `Fired`, of `expected_kind` | becomes `Armed` on the successor kind with a fresh generation, and schedules it |
| `disarm` | any state | becomes `Disarmed`, at terminal cleanup |

**Every other call is a pure diagnostic.** It returns `StaleTransactionDeadline`
and produces no SQL, no reply, no claim change, no interrupt and no state
mutation. `Armed -> Fired` is one atomic flip rather than a check followed by a
mutation, so two deliveries of the same expiry cannot both believe they own it.

`replace_current` accepts `Fired` as well as `Armed` deliberately: a deadline
that has already fired and driven the transaction into forced cleanup must still
be replaceable by the deadline that bounds *that* cleanup, or a hung rollback is
unbounded and strands the admission claim.

The kinds are closed at three:

| Kind | Armed on entry to | Bounds | Replaces |
| --- | --- | --- | --- |
| `Execution` | `Preparing` | everything a caller can see: admission-to-terminal | - (`arm_initial`) |
| `CancellationSql` | `Cancelling` | forced cleanup, from the force winning the gate to the acknowledgement | `Execution` |
| `TerminalSql` | `Settling` | terminal SQL, from issue to answer | `Execution` |

`Cancelling` and `Settling` are the only states that wait on a backend that owes
an answer and has no caller left to give up, and each has its bound. **Every call
site names `Execution` as its expected kind**, so `expected_kind` is not
decoration: a second force arriving in `Cancelling` finds `CancellationSql`
current, fails the expectation, and is a pure diagnostic rather than a second
cleanup with a fresh generation.

When a second-stage deadline fires, the backend did not answer within its grace.
There is no third timer and no escalation: the session is **withdrawn** (the
physical connection destroyed rather than returned) and the transaction settles
as indeterminate, carrying the latched cause. Destroying the connection is the
fence, and it suffices because the admission key is process-local and protects
the *transaction connection slot*, not the server-side transaction. That closes
invariant 3: every fired deadline has a bounded path to `Settled` that requires
no cooperation from the backend.

**Accepted cost, stated rather than hidden.** Closing the socket does not
guarantee the server-side transaction is *already* gone, so a following
transaction for the same app can still block on locks it holds. The cancellation
request sent on entry to `Cancelling` is what bounds that in practice. The
**safety** property - no later SQL on that transaction, and no reuse of a session
in unknown state - holds unconditionally; the liveness of lock release does not,
and belongs to `statement_timeout`.

### Frames and effects

The transaction owns a **strict-LIFO frame stack** (`reducer/frames.rs`). Only
the innermost open frame may issue data SQL, open a child, or close. The root
frame is created only once `BEGIN` is confirmed; a child is inserted as `Opening`
before `SAVEPOINT` is sent and becomes `Open` only when that command succeeds.

A frame's savepoint name comes from a **monotonic sequence**
(`zs_sp_<frame sequence>`), never from the current depth. The simultaneous-open
depth stays capped at eight, which is the existing public limit; the sequence
governs naming only. `ROLLBACK TO` is followed by `RELEASE` of the same name, and
the order is not interchangeable: after a failed statement the subtransaction is
in an aborted state where `RELEASE` is refused and only `ROLLBACK TO` recovers it.

**Effects are per-frame, not per-app** - a flat app-keyed buffer lets a top-level
`COMMIT` drain a rolled-back child's queue and tell a subscriber about a row that
does not exist - and their fate is a property of how the frame closed:

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
transaction is not `Idle` and the frame is not available for new work.

**Ordering is contract, not implementation detail.** The frame's fate is applied
**after** the statement succeeds, never before. Discarding on the assumption that
`ROLLBACK TO` will succeed makes the failure row unachievable: the diagnostic
evidence it calls for is already gone by the time the failure is known.

### Guard order

Which guard runs first decides **which error the caller sees**, and two orders
that accept and reject exactly the same event sequences can still report
different reasons for the same rejection. That makes the order part of the
contract. It runs:

1. **Identity.** Authority domain and incarnation, checked against the routed
   key. A mismatch returns `AppIncarnationMismatch` and **must not touch that
   entry's session, actor, timer or admission**, not even to cancel it. A detach
   request additionally requires the expected authority to equal the stored one,
   and a mismatch there interrupts nothing.
2. **Token match.** Every completion must carry the token its current action
   minted; a wrong one is `StaleTransactionCompletion` and changes nothing.
   Cancellation completions match the entry's single cancellation token, not the
   command token.
3. **Deadline preflight, before claiming the timer.** A fired deadline runs a
   **pure** state/event capability check under the reducer lock first, and only a
   preflight-legal event may claim the fire. Ordering it this way is what stops an
   illegal cell from claiming a timer or queueing a forced settlement as a side
   effect of being rejected.
4. **Backend generation.** A completion naming a generation other than the one the
   admitted handle stored is stale and touches nothing. The counter is a
   thread-level `BACKEND_GENERATION` in `driver.rs` and never goes backwards.
5. **The state/event matrix.** Its not-ready / busy / expired error wins whenever
   the state cannot legally process the event at all.
6. **Only then** the frame guards - top, root, depth - and only inside a state
   where the matrix already marked that event potentially legal. None of them
   mutates the stack: a non-top parent is `SavepointNotCurrent`, a ninth
   simultaneous child is `SavepointDepthExceeded`, closing root is
   `SavepointRootCannotClose`.

Two consequences that are contract rather than style:

- **Opening a frame takes no caller-supplied child id.** The registry mints a
  never-reused frame id and savepoint name from its own sequence. A
  caller-supplied id would let a creator collide two frames or resurrect a closed
  name, and invariant 9's "never repeats" could not then be enforced.
- **Every illegal matrix cell has a type-correct error path.** Reply channels
  carry `Result<_, TxProtocolError>`, so a rejection is a value the caller
  receives, never an out-of-band log line. A protocol whose illegal transitions
  are only observable in a worker log is not executable.

### One gate for every forcing publisher

The deadline is only safe if it cannot race an ordinary completion, and guarding
only caller cancellation guards the wrong set. The forcing publishers are:
explicit `Cancel`, caller-drop cancel, the execution deadline firing, an exact
`DetachRequested`, and a lifecycle observation the classifier has ruled
`ReResolve` or `Deny`. All five go through **one** gate.

The gate has three outcomes, and the middle one is the one a hand-rolled
implementation gets wrong:

- **Force won.** The gate was open; the force latches the single cleanup cause and
  the ordinary completion is suppressed.
- **Completion won.** An ordinary completion was already queued. The force is
  enqueued *after* it rather than replacing it, and the reducer processes the
  ordinary result **first**, then enters `Cancelling` from the state that results,
  with the cleanup goal that state fixes. The goal is read at entry, not at the
  moment the force was published.
- **Joined.** An earlier force already owns the cause. The second one joins it and
  emits nothing. **Exactly one cleanup cause is ever latched**, so the reason a
  transaction ended is deterministic rather than last-writer-wins, which matters
  because that cause is what the creator is told.

Two rules make this airtight and both are load-bearing:

1. **Lock order is fixed: command gate, then terminal owner. The reverse is
   forbidden.** That ordering is why a completed terminal owner can never coexist
   with a still-open command gate that a deadline could then flip to forced.
   Without it the two guards are individually correct and jointly useless.
2. **Once a terminal completion is promised, a later forcing publisher neither
   sets a new force nor suppresses the promised result.** It is treated as a late
   cancel awaiting `AlreadyCompleted`. A deadline that fires microseconds after a
   commit succeeded must not turn that commit into a cancellation.

This applies in `Preparing`, `Starting`, `Idle`, `InFlight`, `Quiescing` and
`Poisoned` alike. `Idle` is not an afterthought: rule 4's central case, a callback
that never settles, fires against a transaction sitting in exactly that state.

**`Settling` is the one nonterminal state a force cannot claim**, and rule 2 is
why. Terminal SQL has been issued exactly once and the backend owes an answer; a
force arriving now would start a competing cleanup on a session that is already
ending. It is a late cancel: it joins the terminal waiters and changes nothing.
The `TerminalSql` deadline bounds that wait, which is also why `replace_current`
never has to accept `TerminalSql` as an expected kind.

### The invariants a property test asserts

These say what must hold after **every** generated prefix, including prefixes
ending in an illegal event. A pure reducer driven by a model backend is what makes
them checkable without a database.

1. **Qualified identity.** An event for authority A never mutates, interrupts,
   settles or executes SQL for B. Epoch mismatch re-resolves; domain or
   incarnation mismatch denies terminally.
2. **Admission cardinality.** At most one nonterminal entry owns an admission key.
3. **Claim balance.** Every granted admission has exactly one release, and only
   once no live transaction or reservation remains. This holds unconditionally
   only because every state that waits on a backend is bounded and every bound
   ends in withdrawing the session rather than in waiting longer.
4. **Session conservation.** Between confirmed `BEGIN` and terminal cleanup,
   session ownership is exactly one of the registry, the matching command token,
   or **withdrawn**, never silently absent. **`Withdrawn` is a session-ownership
   value, not a transaction state**: the session is never returned to the
   registry, never leased to another command, and its physical connection is
   destroyed at terminal cleanup rather than reused. Closing the connection *is*
   the proof that nothing further can run on it, so no supervisor, retirement
   proof or quarantine state is required. The failure it prevents is the one the
   driver names: handing the next user of a connection an aborted transaction.
5. **Single command.** At most one active backend token per transaction;
   duplicate or stale completions cannot alter state or effects.
6. **Operation serialization.** A second operation while one is in flight returns
   `transaction_connection_busy`, never silently deferred, never silently
   autocommitted.
7. **Authority separation.** Every data-SQL trace is preceded by a successful
   platform-role authority read for the same app authority, and **no authority
   read is ever on the tenant data session**.
8. **Ceiling monotonicity.** The effective ceiling is never broader than the
   `BEGIN` ceiling or any previously accepted value.
9. **Frame stack.** Root at index zero, parent links form one chain, only the top
   acts, simultaneous child depth at most eight, and **a frame sequence or name
   never repeats**.
10. **Complete child close.** Every child that closes normally has exactly one
    `RELEASE`; a rolled-back child has `ROLLBACK TO` **before** `RELEASE`.
11. **Effect locality.** Success appends only to the current frame; release moves
    the exact child sequence to the parent; a confirmed rollback-to discards
    exactly the child sequence **and no parent effect**. When a frame's watermark
    is missing the buffer is left alone rather than truncated to zero, because
    truncating would discard parent effects.
12. **Commit-only publication.** Nothing publishes unless the root **intent was
    commit** AND the root finish result was committed, and a confirmed commit
    publishes every retained effect exactly once, in order. A `Committed` result
    returned against a *rollback* intent is a terminal-result **mismatch**, not a
    commit: it publishes nothing.
13. **Poison rule.** No data, frame-open or release command may start from
    `Poisoned`. A successful rollback-to or release of the recovery child returns
    the parent to `Idle`; otherwise **only root settlement** can end it.
    `Poisoned` is therefore *recoverable by construction*, which is why no forced
    cleanup may route through it (invariant 16).
14. **Poisoned commit.** A `COMMIT` answered `ROLLBACK` never resolves a creator
    promise and never publishes an effect.
15. **Quiescing.** Settlement arriving while another command owns logical
    execution issues **zero** frame or terminal SQL until that command returns; it
    then starts at most one logical settlement attempt, and never more than one
    SQL statement concurrently. A successfully closed rollback-to frame settles as
    its ordered `ROLLBACK TO` then `RELEASE` pair; if the `ROLLBACK TO` fails, the
    legal error row stops **without** `RELEASE` and retains the frame's effects,
    then takes exactly one of **three** dispositions, chosen by the same health
    oracle the cleanup goals read:

    | The oracle says | Disposition |
    | --- | --- |
    | a transaction is still open and in error | **poisons**; invariant 13 governs it from there |
    | no transaction remains - the backend ended it | **terminal**. Settle as aborted-by-backend, discard every frame buffer, release the claim. No `Cancelling`: there is nothing left to cancel |
    | it cannot say | `Cancelling` with goal `AbortIfOpened`, and the session is **withdrawn** per invariant 4 |

    There is no fourth. The three exhaust the oracle's range, which is what makes
    this checkable rather than a list of remembered cases.
16. **Forced cleanup is terminal.** Once `Cancelling` is entered, no path leads
    back to any state that can issue creator data SQL, and exactly one cleanup
    cause is ever latched. Every exit is `Settled`.

Invariants 13-16 decide whether the state table is executable or merely
descriptive. 1-12 constrain what a transition may do; these constrain what may
happen when two things arrive at once.

### Acceptance shape

**Every plugin-db test target declares `required-features`** (all eight in
`crates/zeroship-data-v8/Cargo.toml`), so a plain `cargo test -p
zeroship-data-v8` filters them all out and never builds them. Citing an arm
without naming its invocation is how "already covered" comes to mean "compiled by
nobody's routine command". The transaction target runs as:

    cargo test -p zeroship-data-v8 --test native_transaction \
      --features test-helpers -- --test-threads=1

The pure arms live in
`crates/zeroship-data-orm/src/transaction/reducer/tests.rs` and
`crates/zeroship-data-orm/src/transaction/reducer/frames.rs`, and run under
`cargo test -p zeroship-data-v8 --lib`. The live arms live in
`crates/zeroship-data-v8/tests/native_transaction.rs` and need PostgreSQL.

The required arms, and where each lives today:

| Arm | Where |
| --- | --- |
| a settle while a command owns execution issues zero SQL, then terminal SQL exactly once | `a_settle_while_a_command_owns_execution_quiesces_and_issues_no_sql` |
| a force never routes through `Poisoned` | `a_force_never_routes_through_poisoned` |
| a forced transaction cannot be resurrected by `rollbackTo` | `a_forced_transaction_cannot_be_resurrected_by_rollback_to` |
| a second-stage deadline settles rather than hanging | `the_second_stage_deadline_settles_rather_than_hanging` |
| a deadline firing after a commit succeeded does not cancel it | `a_deadline_firing_after_a_commit_succeeded_does_not_cancel_it` |
| a force cannot claim `Settling` | `a_force_cannot_claim_settling` |
| every granted admission has exactly one release | `every_granted_admission_has_exactly_one_release` |
| a FAILED `ROLLBACK TO` retains the frame's effects | `a_failed_rollback_to_retains_the_frames_effects_for_diagnosis` |
| a rolled-back frame's effects are never published | `a_rolled_back_frames_effects_are_never_published_by_the_root_commit` (pure) and `savepoint_rollback_must_not_publish_its_change_event_at_outer_commit` (live) |
| a savepoint name is never reused while a leftover of that name can exist | `a_savepoint_name_is_never_reused_at_the_same_depth`, `dispatch_emits_the_reducers_monotonic_savepoint_names`, and `a_rolled_back_savepoint_stays_defined_and_a_reused_name_shadows_it` |
| a `COMMIT` answered `ROLLBACK` is a failed transaction | `commit_that_postgres_rolled_back_must_not_report_success_l8` |
| a forced cleanup on a poisoned block keeps a healthy connection | `a_forced_cleanup_on_a_poisoned_block_keeps_a_healthy_connection` |
| a forced cleanup cancels the running statement and keeps the connection | `a_forced_cleanup_cancels_the_running_statement_and_keeps_the_connection` |
| a withdrawn session never comes back from the pool | `a_withdrawn_session_never_comes_back_from_the_pool` |
| a deadline that fires in `Preparing` settles without a `BEGIN` | `a_deadline_that_fires_in_preparing_settles_without_a_begin` |
| a classified setup failure does not collapse into `begin_failed` | `a_classified_setup_failure_does_not_collapse_into_begin_failed` |

The `Promise.all` case has a black-box probe (`todos.txParallel` in
`examples/db-todos/src/index.ts`, asserted by `cxPar` in
`examples/db-todos/tests/database.test.ts`). Keep it as a **preservation property**: it is
not acceptance for this contract, because the durability half cannot fail and it
would pass on an implementation that keyed admission by transaction id. The
discriminating observable is the exclusion itself, which is a white-box arm on
the reducer.

## Why it is this way

**Terminal delivery does not survive process death, deliberately.** There is no
durable transaction registry and no durable fence-job system, and neither is a
deliverable of this contract. The only candidate that could force one is a
workflow step, and the workflow layer already owns that failure mode: durable
workflows keep a journal (`StepCheckpoint` / `StepOutcome` / `StepResult` in
`crates/zeroship-control/src/cron/workflow_engine.rs`, `JournalStepRecord` in
`sdks/workflows/src/journal.ts`) and refuse I/O outside a journaled step by
construction. A creator transaction inside a workflow therefore runs inside a
journaled step, and process death is handled by replay against that journal.
The accepted cost is that such a transaction is **at-least-once**: if the step
commits and the process dies before the journal records the outcome, replay
re-runs the step. That is the workflow layer's idempotency contract and belongs
in `docs/reference/workflows.md`. The decision is reversible in the cheap
direction: a registry can be added later without redesigning the reducer.

**Two same-app top-level transactions serialise, and that is user-visible.** It
decides what `Promise.all([db.transaction(a), db.transaction(b)])` does. A
black-box suite underdetermines it: a suite that never states whether the two
serialise passes just as happily on either answer. Only one transaction
connection slot exists per admission key, so a second concurrent top-level
transaction has nowhere to live: without the wait it evicts the first (Postgres)
or is refused by the single SQLite writer. Waiting turns both into "runs second
and succeeds", which is what the creator meant.

**A bounded set of SQLite transaction connections is rejected**, for three
reasons that compound: deploy-pinned isolates exist only in the worker and the
worker refuses SQLite DSNs outright (`crates/zeroship-worker/src/main.rs`), so
the SQLite tier has one isolate per app by construction and extra connections
would buy concurrency that tier cannot produce; SQLite permits one **writer** per
database regardless, so they would buy concurrent readers and nothing more; and
they would trade deterministic queueing for `SQLITE_BUSY_SNAPSHOT` nondeterminism
on write upgrade while pinning WAL read marks.

**Nine states, not five.** `Poisoned`, `Preparing`, `Quiescing` and `Cancelling`
each name a condition a shorter list has to fake somewhere else: as a bookkeeping
flag beside the state, as an early `Starting` that has already issued `BEGIN` on
an authority nobody checked, as an empty client slot, or as a `Settling` that
never issued the SQL its own definition promises.

**Forced cleanup owns a state, and routing it through `Poisoned` is unsound.**
Three separate reasons, each sufficient:

1. **`Poisoned` is recoverable, and a force must not be.** Invariant 13 lets a
   successful `ROLLBACK TO` of the recovery child return the parent to `Idle`. A
   transaction parked in `Poisoned` by an expired deadline or by a
   `Deny(AuthorityDomainMismatch)` verdict could then be walked back to `Idle` by
   the creator's next `rollbackTo` and resume issuing data SQL, under an authority
   the classifier terminally denied and past a deadline that already fired. That
   is a privilege defect, and it follows from this document's own invariant.
2. **`Settling` promises SQL that a force has not sent.** A force winning in
   `Preparing` issues no `BEGIN` and therefore no terminal SQL at all; a force
   winning in `Starting` does not know whether a transaction exists to end.
3. **A force in `InFlight` or `Quiescing` cannot issue terminal SQL.** A command
   owns the session and invariant 5 permits one active backend token. Waiting for
   that command to return is what `Quiescing` does for a cooperative settle, and
   is precisely what a force must not do: the deadline fired *because* the command
   is not returning.

**WHEN the health oracle is sampled is load-bearing.** On PostgreSQL
`transaction_status()` returns `None` while a request is in flight, and a *failed*
statement's trailing `ReadyForQuery` is not consumed when its `await` returns.
Inside a poisoned block, where every data statement fails with `25P02`, **no
retry makes the oracle answer**. Since `None` is indeterminate and indeterminate
withdraws the session, a driver that samples on entry to `Cancelling` would
withdraw a perfectly healthy connection on *every* forced cleanup. So the oracle
is sampled **after** the cleanup `ROLLBACK`, which succeeds and resolves the
byte. This is a constraint on the driver, not on the reducer, which is why it
belongs in this contract rather than being left for whoever writes the driver.

**Savepoint names are monotonic because the server leaves rolled-back savepoints
defined.** `ROLLBACK TO SAVEPOINT` deliberately leaves the savepoint defined, and
PostgreSQL resolves a savepoint name to the **most recently established** one. A
depth-derived name is reused after the depth decrements, so a leftover savepoint
shadows an enclosing frame of the same name and sends the enclosing rollback to
the **wrong scope**. It also leaves one open subtransaction per rolled-back
savepoint, which a retry loop accumulates. Monotonic naming removes the shadowing
precondition outright rather than relying on every cleanup path succeeding.

**The denial reasons stay distinct all the way to the caller.** Collapsing them
would make the only distinguishable signal a log line the creator cannot read,
turning the next action into a guess. The reverse choice also leaks more: a single
collapsed error hides a *permanent* condition behind a *retryable-looking* one, so
the observable difference migrates from an error code into retry timing, which
every caller can measure and no caller can act on correctly. What the distinction
leaks is nothing: `Deny` is only ever returned to a caller whose identity
*matched*, because identity is compared first, so `APP_DEPROVISIONED` tells an app
that its own authority record carries a tombstone. It is not an oracle over other
tenants.

**What this contract does not decide.** How SQLite's actor honours these states is
SC-2's: cancellation and rollback here assume an actor that can be interrupted,
which it is - `TxCanceller` reaches `SqliteCancelHandle::cancel`, which records
the intent and interrupts the actor's connection through `Interrupts::interrupt`.
The plan-level atomicity randomized encryption needs is SC-3's.

## Open

1. **The authority record has no producer, so the classifier is a tautology in
   production.** `driver::expected_authority` mints incarnation `0`, domain
   `(0, 0)` and epoch `0`, and `driver::observation_for` echoes that expectation
   back as the observation with `LifecycleState::Stable` and an empty ceiling.
   `classify` can therefore only return `Current` on any production path, and
   `APP_DEPROVISIONED`, `STALE_APP_INCARNATION` and `AUTHORITY_DOMAIN_MISMATCH`
   are unreachable outside tests. Everything downstream ships and is tested: the
   classifier, all three verdict arms, the retryable wire error, the session-setup
   adapter and both rotation directions. What is missing is the record: who
   publishes an app's lifecycle state, incarnation, authority domain and schema
   epoch, and where the worker reads it from. The worker may only READ it, so the
   producer must be a service that does not execute creator code.
   **NEEDS-DECISION.**

2. **Creator statements do not reach the reducer's operation events.**
   `driver::run_operation` carries `#[allow(dead_code)]` and has no production
   caller; creator CRUD issued inside a transaction takes the session through
   `TxClientSlotGuard` / `take_tx_client_for` in `exec.rs` and reports nothing to
   the state machine. Consequences: `InFlight`, `Quiescing` and `Poisoned` are not
   reached by creator statements, and a failed creator statement leaves the
   reducer reading `Idle` while PostgreSQL reads `Failed`. Forced cleanup still
   behaves correctly, because `OpenTransaction` is fixed from `Idle` and
   `Poisoned` alike and the health oracle is sampled from the server rather than
   from the reducer. Two production call sites in `exec.rs` (`run_sql`'s in-tx arm
   and the SQLite arm) must route through `run_operation`, and the in-crate tests
   that install a transaction session with no reducer behind it must install one.
   **BUILDABLE, 6h.**

3. **Should SQLite's backend-level refusal become a bounded wait, now the deadline
   ships?** `reserve_transaction`'s own comment defers the question to this
   contract: a refusal needs no deadline to be safe, which is why it was chosen
   before there was one. Admission already makes a creator's second same-app
   `db.transaction()` wait, so the refusal is reached only by a caller that
   acquires without the claim; the question is whether the backend should still
   hold a policy that contradicts the one above it. Changing it also changes
   `docs/reference/sqlite-divergences.md`. **NEEDS-DECISION.**

4. **Does the admission key need the runtime instance, or is the OS thread
   enough?** The specification says `(runtime instance, app, incarnation)`; the
   tree keys a thread-local lane map by app id, so two isolates of one app on one
   thread share a lane and serialise where the specification would let them run
   concurrently on Postgres. That is strictly safer and matches SC-5's
   thread-resource model, but it is not what this document specifies. Either
   narrow the key or narrow the specification. **NEEDS-DECISION.**

## History

Deliberation lives in `docs/proposals/2026-08-26-runtime-db-binding-design.md`
(the parent) and `docs/proposals/2026-08-26-runtime-db-binding-decision-log.md`.
Defects cited by `L` number are in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md` or, once
closed, in `docs/proposals/2026-08-26-runtime-db-binding-defects-closed.md`. The
`DBR-` labels are defined in `docs/reviews/2026-08-26-plugin-db-review.md`;
DBR-03 (an absent client read as proof terminal SQL ran) and DBR-11 (the leaked
admission claim) are both fixed and their names survive only as comments at the
code that closed them.

The exhaustive legal-transition table, illegal-transition matrix and typed error
per cell live in `docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md`. Work from
that for the matrix; work from this document for the contract. Where the two
disagree, the invariants above are the arbiter, because they are the part a
property test can check.

DO NOT:

- **Do not build a durable transaction registry, a `FenceJobRegistry` or durable
  fence jobs.** The r7 artifact names all three; the workflow journal already owns
  cross-process terminal delivery, and building one underneath it is a second
  durable job system.
- **Do not adopt `HardStopping`, `Quarantining`, the `QuarantineUnknown` cleanup
  goal or a fifth deadline mutator (`replace_with_retirement`).** Each pulls in
  the supervisor and the signed retirement proof this contract declines.
  `QuarantineUnknown`'s content is "we do not know whether a transaction is open",
  which is what `AbortIfOpened` already says. The artifact's own claim that there
  are five mutators is not a closure claim to inherit: it omits two its own states
  call.
- **Do not key admission by transaction id.** It removes the `Promise.all`
  serialisation silently, and a test asserting only that both writes land stays
  green.
- **Do not route forced cleanup through `Poisoned`, and do not report a force as
  `Settling`.** See the three reasons under "Why it is this way". Routing through
  `Poisoned` passes every acceptance arm except the resurrection one.
- **Do not sample the PostgreSQL health oracle before the cleanup `ROLLBACK`.** It
  reads `None` inside a poisoned block and destroys a healthy connection on every
  forced cleanup.
- **Do not answer a slow statement by destroying the connection.** Forced cleanup
  used to withdraw first; the execution deadline fires *because* a statement is
  slow, so the mechanism that bounds a slow statement was killing the backend in
  its own common case. Cancel, reclaim, roll back, then sample.
- **Do not derive savepoint names from the current depth.** A leftover savepoint
  of the same name shadows an enclosing frame.
- **Do not broaden the terminal-tag check past the PostgreSQL `COMMIT` arm.**
  `RELEASE` answers with the tag `RELEASE`, so "anything but COMMIT is a failure"
  rejects every healthy nested commit.
- **Do not truncate the effect buffer to zero when a frame's watermark is
  missing.** Truncating discards parent effects; over-publishing is a bug and
  silently dropping a committed row's event is a worse one.
- **Do not assert "a rolled-back frame's effects are never published" without a
  live subscriber.** The emit path returns early unless
  `broker::has_subscribers(app, collection)`, so without one the arm passes
  vacuously against the very defect it exists to catch.
- **Do not give `BACKEND_GENERATION` a `reset_for_tests`.** It lived on
  `ThreadDbContext`, where the test reset rebuilt the struct and silently returned
  the counter to `0`, contradicting the "never reused" its own doc claimed.
- **Do not drop the withdrawal tombstone in favour of a flag inside the lane.** A
  withdrawn session can be out of the slot in another future's hands, and that
  future's `Drop` returns it to a lane that no longer exists; a flag inside the
  lane dies with the lane and lets a condemned session reach the pool.
- **Do not cite a plugin-db test arm without its invocation.** All eight test
  targets declare `required-features`, so the routine command builds none of them.
