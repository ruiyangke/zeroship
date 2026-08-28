# SC-1: the explicit transaction protocol

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Read the set from:**
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`. Live defects cited
here by number (L8, and the `L` numbers generally) live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`.

**Gates:** step 9 of that document (the owned transaction registry and state
machine) and the randomized-encryption atomicity that depends on it.

It no longer gates the L8 fix: that landed on main in `42c925de7` while this
document was being written, so the settle path already judges its terminal
command tag. This contract inherits that behaviour rather than introducing it.

---

## Known scope divergence: the executable form is larger than this contract

An exhaustive, executable form of this protocol was written out against this
design and is preserved at `docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md`
(untracked, beside these drafts). "Where the full transition matrix lives",
below, says why that matrix is not inlined here. This section says the thing a
reader has to know before either document is used for planning: **the two
disagree about how large the design is.**

Measured 2026-08-27, `grep -cE 'supervisor|FenceJobRegistry'` over that
12,113-line artifact matches **122 lines**. They invoke a *supervisor*, a
`FenceJobRegistry`, and durable fence jobs with a recovery scan - a component
that owns terminal delivery so that "actor/publisher death is irrelevant". The
same expression over this contract matches only the lines of this section,
which name the component and specify none of it, and the codebase contains
**zero** occurrences of `FenceJobRegistry`.

An earlier draft of this passage gave the figure as "105 lines" and said this
contract "contains **zero** lines mentioning either word". Neither survived
re-measurement: the count did not reproduce, and the zero-lines claim was
falsified by the paragraph that made it. The instrument is named above so the
next reader re-runs it instead of inheriting it. The codebase half held.

That is not a criticism of the artifact. Making the protocol genuinely
executable across process death plausibly *does* require durable terminal
delivery, and it is better to learn that from a document than from an outage.
But it means the artifact is not "SC-1 written out in full" - it is **a
substantially larger system than SC-1 describes**, resting on infrastructure
that neither exists nor is budgeted anywhere in this design set.

Two consequences, and the second is the one that matters for sequencing:

1. This contract owes an explicit answer on whether terminal delivery must
   survive process death at all. If it must, a durable job registry is a named
   deliverable with its own storage, its own supervisor and its own failure
   modes - not an implementation detail of the reducer. If it need not, the
   artifact's cutoff machinery is over-built and the simpler in-memory gate
   described below is the design.
2. **A scope divergence this size is exactly what a step estimate misses.** The
   implementation step for SC-1 currently reads as "write the reducer". On the
   artifact's reading it is "write the reducer, plus a durable job system".
   Discovering that during implementation rather than here is how a step that
   was scoped as days becomes weeks.

### Evidence bearing on that question, gathered 2026-08-28

The open question is *who is waiting for the terminal verdict*. If nothing
durable awaits it, the in-memory gate is the design and the artifact's cutoff
machinery is over-built.

**The one candidate that could have forced a durable registry is a workflow
step, and it does not - because the layer above already carries the durability.**

- Durable workflows keep a **journal**: `StepCheckpoint`, `StepOutcome` and
  `StepResult` (`crates/zeroship-control/src/cron/workflow_engine.rs:35`),
  `JournalStepRecord` (`sdks/workflows/src/journal.ts:64`).
- **All I/O is forced inside a journaled step.** The workflow body refuses
  direct I/O by construction - *"workflow bodies may not perform I/O directly -
  move fetch(...) inside step.run(...) or use step.sideEffect(...)"*
  (`journal.ts:30`), with the same treatment for timers (`:32`).
- `crates/zeroship-plugin-workflow/src/` contains exactly **one** `transaction`
  reference, `client.transaction()` at `claim.rs:68` - the platform's own claim
  record, not a creator's `db.transaction()`.

So a creator transaction inside a workflow runs **inside a journaled step**, and
process death is handled by replay against that journal. Adding a durable
`FenceJobRegistry` to this contract would put a **second durable job system
underneath one that already exists** and already owns this failure mode.

**Therefore the recommended answer is NO: terminal delivery need not survive
process death, and the simpler in-memory gate below is the design.**

**The caveat, stated rather than fixed here.** If a step commits and the process
dies before the journal records the outcome, replay re-runs the step - so a
transaction inside a step is **at-least-once**. That is the workflow layer's
idempotency contract and belongs in `docs/reference/workflows.md`, not in this
protocol. Solving it here would be solving it in the wrong layer, and twice.

**Reversibility is why this is the cheap answer to give now:** the registry can
be added later without redesigning the reducer. The reverse - building it and
discovering the journal already covered the case - cannot be undone as cheaply.

*Unverified: whether any NON-workflow durable consumer awaits a transaction
terminal. I checked the workflow path because it was the strongest candidate;
I did not enumerate every caller.*

### Terms this contract uses but does not define

Four pieces of vocabulary appear in the rules, the guard order and the
invariants below without ever being defined - not here, and not in any other
document of this set. They are enumerated rather than defined, because defining
them is a decision this document has not made:

- **`Preparing`** and **`Quiescing`** are used as protocol states ("One gate for
  every forcing publisher" applies the gate "in `Preparing`, `Starting`,
  `InFlight` and `Quiescing` alike", and invariant 15 is *titled* Quiescing).
  Neither appears in the diagram or the list under "States", which names six:
  `Starting`, `Idle`, `InFlight`, `Poisoned`, `Settling`, `Settled`.
- **`ReResolve` / `Deny` / `Current`** are the lifecycle classifier's verdicts,
  named in the forcing-publisher list under "One gate for every forcing
  publisher", which also has the reducer re-run the classifier. The classifier
  that produces them is specified nowhere in this set.
- **`Armed` / `Fired`** are the deadline slot's two states, named in guard step
  3 together with a `(kind, generation)` pair, an atomic claim and a diagnostic
  outcome for a wrong pair. The slot's own protocol is not written down here.

All four are specified in the r7 artifact named above, which is where they were
drawn from. That artifact is untracked, so this contract currently rests on
vocabulary a reader of the tracked set cannot resolve.

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
semantics above. **The key differs by backend, and that is Fork A's decision
rather than an oversight**: SQLite serializes across isolates sharing one actor,
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

### What this contract does not decide

- Whether SQLite's actor can honour the same states. That is **SC-2**, and it
  must land first for the SQLite arm: cancellation and rollback here assume an
  actor that can be interrupted, which today's cannot.
- The plan-level atomicity that randomized encryption needs (DBR-04/05). That
  depends on **SC-3**'s grammar.

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

  **A `COMMIT` from `Poisoned` is reachable and must be handled, not
  forbidden.** An earlier draft said "only `ROLLBACK` is legal" and then, four
  lines later, described a `COMMIT` being sent from this state - a
  contradiction. The
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
   (`transaction/mod.rs:1034-1039`). That is DBR-03. Under this contract, a
   settle arriving while the state is `InFlight` **waits for the operation to
   return the client**; only a state of `Settled` ends a settle early.

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
   transactions for that app parked indefinitely.

   That defect is labelled **DBR-11** here, and the label resolves to nothing:
   `DBR-11` occurs in no other document of this set, and the defect has no `L`
   number in the register. DBR-03, DBR-04/05 and DBR-06 at least reappear in the
   design document; DBR-11 does not. The label is kept rather than invented
   away, because dropping it would lose the only handle the defect has - but it
   is **owed** either a definition of the DBR numbering or a register row.

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

## Where the full transition matrix lives

This document states the *contract*: the states, the load-bearing rules, the
guard order, the gate protocol and the invariants. It deliberately does **not**
inline the exhaustive legal-transition table and illegal-transition matrix -
several hundred rows of `(state, event) -> action, next state, typed error`.
That belongs with the implementation, next to the `match` arms it becomes, not
in a contract someone has to read end to end.

The exhaustive version exists and was produced against this design: the closed
state enum, the closed event and completion enums, the complete legal transition
table, the exhaustive illegal matrix with a typed error per cell, the deadline
slot protocol, and the terminal outcome table. It is preserved at
`docs/reviews/dbbind-2026-08-26/dbbind-r7-codex.md` (untracked, beside these
drafts). An implementer should work from that; a reviewer should work from this.

If the two ever disagree, **this document is not automatically right** - it is
the more readable one, which is a different property. The invariants below are
the arbiter, because they are the part a property test can actually check.

They already disagree, and by how much is recorded at the top of this document
under "Known scope divergence".

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
   flips it to `Fired` atomically. A wrong pair is a pure diagnostic. Ordering
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
  are only observable in a worker log is not executable, and this document has
  already been bitten once by a diagnostic that went to a discarded stream.

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
  ordinary result **first**, then starts a fresh rollback from the state that
  results. It does not discard a completion that already happened, and it does
  not pretend the force arrived first.
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

This applies in `Preparing`, `Starting`, `InFlight` and `Quiescing` alike.
Scoping it to one state is the same mistake as scoping it to one publisher.

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
14. **Poisoned commit.** A `COMMIT` answered `ROLLBACK` never resolves a
    creator promise and never publishes an effect. This one is not speculative -
    it is defect **L8 in the defect register**, and its reachable case is
    already covered by a live regression test
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
    retains, poisons or quarantines as classified.

Invariants 13-15 are the ones that decide whether the state table is executable
or merely descriptive. 1-12 constrain what a transition may do; these constrain
what may happen when two things arrive at once - a settlement racing a command,
a commit racing a poison - which is exactly where a hand-written implementation
diverges from its own table.

Invariant 11 is the one the shipped fix already leans on: when a frame's
watermark is missing the buffer is left alone rather than truncated to zero,
precisely because truncating would discard parent effects. Over-publishing is a
bug; silently dropping a committed row's event is a worse one.

## Acceptance shape

### The invocation these arms require

Every citation of a `native_transaction` arm in this document - invariant 14's
included - **only counts under a specific invocation**, and this is exactly the
trap the fourth defect class describes - "A fourth: the arm that was never
built", in
`docs/proposals/2026-08-26-runtime-db-binding-verification-record.md`.
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
  This document's "Why this is a document and not a discovery" argues that a
  black-box suite cannot see this property; leaving the arm in the weaker form
  conceded exactly that point.

  **It is worse than "cannot fail in principle" - it is already written, and
  already green.** The probe exists (`examples/db-todos/src/index.ts:569-599`
  starts the pair in one `Promise.all`) and the dev-vs-deployed gate already
  asserts all three halves of it:
  `want cxPar 'two transactions in one Promise.all: leg 1 commits'`,
  `... leg 2 commits`, and `... both concurrent transactions left their row`
  (`tests/e2e_dev_vs_deployed_db.sh:1215-1217`). Those verdicts were RED when
  first written on 2026-08-10 and are green now, so they were a real measurement
  of the gap they were built for - and are worth **keeping as a preservation
  property** for exactly that reason. What they cannot be is acceptance for the
  work this document proposes: they would stay green if none of it were done.

  So the arm needs a **discriminating partner**, and the partner has to be
  named concretely rather than gestured at. Two forms qualify:

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
  following transaction for the same app succeeding.
