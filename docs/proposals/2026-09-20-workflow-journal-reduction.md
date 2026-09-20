# Reducing the workflow journal to the state it needs

**Status.** PROPOSED. Nothing here is built.

This is a sibling of `docs/proposals/2026-09-19-workflow-journal-relocation.md` and depends
on it. That document moves the journal into `workflow_manager`. This one says what the journal
should look like once it is there, and argues that most of what it currently carries exists to
solve a problem the move deletes.

Pre-launch, so nothing here needs a migration path. The old shape disappears in the same change
as the new one.

---

## What is true today

**The journal's tables fall into groups, and they are not equally load bearing.**
`crates/zeroship-workflow-schema/schema/schema.ts` declares them. Grouped by what they serve:

```
  execution        runs, generations, steps, tasks, waits, signals, payloads, payload_refs
  delivery         job_publications, advance_publications, fanout_publications,
                   propagation_publications, fanout_pages, propagation_pages
  cancellation     propagations
  event log        outbox
  bookkeeping      job_receipts, management_receipts, requests,
                   collection_pages, reconciliation_pages
  pub/sub          topics, broadcasts, subscriptions
  deployment       deploys, deployment_holds, activations, activation_scopes
  continuation     continuation_heads, continuation_members
  schedules        schedules, occurrences
  housekeeping     schema_version, app_state
```

**The delivery group is there for one reason.**
`crates/zeroship-workflow/src/service/publication.rs` opens by saying so:

> Creator-owned, immutable queue publication intents. Journal transitions write intents before
> COMMIT. Network publication happens after it; only a matching manager receipt confirms an
> intent. Neither history collection nor an unknown remote outcome retires this deduplication
> state.

That is a transactional outbox, and it exists because the journal is in the creator's database
while the durable queue is in `workflow_manager`. Two databases, no distributed transaction, so
a transition cannot commit its own queue effect. Hence an intent, a network publication, a
receipt, and deduplication state that nothing is allowed to retire.

**The modules that serve delivery are a large part of the service.** `delivery`, `publication`,
`fanout`, `propagation`, `collection` and `reconciliation` under
`crates/zeroship-workflow/src/service/` are together comparable in size to `journal`, `runner`,
`tasks`, `signals` and `continuations`. As much of the engine moves messages and resumes scans
as runs workflows.

**One row is the concurrency unit for every workflow operation an app performs.**
`lock_app_state` in `crates/zeroship-workflow/src/service/app.rs` is an UPDATE against the single
`app_state` row for an app, so it takes that row's lock, and it is called from `delivery`,
`ingress`, `app`, `reconciliation`, `management`, `hold_release`, `cron`, `control`, `collection`,
`activation`, `tasks`, `signals`, `propagation`, `fanout` and `closure`. Those subsystems were
designed separately and every one of them converges on the same row.

The consequence is that an app's broadcast waits behind its own journal maintenance. A fan-out
page cannot begin while a payload sweep holds the row, and neither can begin while a signal is
being ingested. Nothing here is a defect: the lock is doing what it was written to do, which is
to serialise an app's transitions. But serialising EVERY subsystem on one row is a throughput
ceiling nobody chose, and it is invisible in each subsystem's own source, because each one takes
the lock once and correctly.

This is a decision the relocation cannot avoid making. Moving the journal into a service either
inherits this serialisation, splits it, or entrenches it, and it is far cheaper to decide before
the creator calls are carried across than to discover afterwards that one row is the throughput
ceiling for a platform. The relocation proposal is currently silent on it, which is a decision by
default rather than by choice.

**Lease validity has two representations and they are not kept apart.** The durable form is
already right: `__zeroship_workflow_tasks.deadline` is an absolute integer, as are the manager's
`assignments.expires_at` and `workers.expires_at`, and `Clock::sample` in
`crates/zeroship-workflow-manager/src/clock.rs` reads the DATABASE clock rather than a local one.
But `Instant` escapes the enforcement site: `Lease` and `Budget` in
`crates/zeroship-workflow-manager/src/queue.rs` hold one, and `Validity::Until` in
`crates/zeroship-workflow/src/service/policy.rs` holds one. An `Instant` is process-local by
construction, so two replicas holding one cannot compare them at all.

---

## What the relocation already removes

Once the journal and the queue are in `workflow_manager`, a transition and its queue effect
commit in one transaction. There is no remote to publish to, so there is no intent to record and
no receipt to match.

What the move does not do on its own is retire the deduplication identity, and that difference
decides how much of the delivery group can go. `publication.rs::record_job` mints a job id with
`JobId::mint`, then looks for an earlier job for the same frontier by selecting on
`(app_id, run_id, generation, frontier_revision, available_at)` and reusing the id it finds. The
identity of a publishable job is that tuple of business columns, not its key, and the tuple lives
only in the per-kind table: the intent row carries an opaque encoded `specification` and nothing
to match on. The unique index over the tuple is what makes select-then-insert correct when two
transactions run it at once, because both can select nothing and both can insert. Each commits
atomically and the frontier still gets two jobs unless something refuses the second.

Nothing on the manager side reconstructs that mapping. `Queue::existing` in
`crates/zeroship-workflow-manager/src/queue.rs` loads by job id and compares a digest, so it can
tell that a job with a given id has the content it should; it cannot find a job with the same
content under a different id. The `jobs` table carries scope, dispatch and management-request
keys and no content-keyed unique index. Both halves of the identity question therefore rest on
the creator-side read-then-mint and the per-kind index behind it.

So one transaction removes the network hop and the receipt. It does not remove the need for a
uniqueness constraint over the identifying tuple, and a design that drops the per-kind tables has
to say where that tuple goes.

This is worth stating plainly because the relocation has been argued as a safety change: a
creator should not be able to drop the log the platform is executing against. It is also a
simplification, because the delivery group exists as a direct consequence of the split it
closes rather than as something workflow execution needs.

---

## What comparable systems do

Both of these were read from source, not from summaries.

**Temporal keeps one outbox and discriminates.** Its per-category task tables are marked in
`common/persistence/sql/execution_tasks.go` as existing for backward compatibility; every
category added since goes into two generic tables keyed by a category id. Paging is not a table
at all: queue readers keep positions in memory and checkpoint an ack level into the shard
record. Child workflows and external signals are not special cases; both are an event, an entry
in a per-execution map, and one outbox row, written atomically.

Two consequences for this proposal, and the second corrects it.

The per-category shape Temporal keeps only for compatibility is the shape we have, with nothing
to keep it for: we are pre-launch and owe no one a migration. That is the fold described below.

An ack level works for Temporal because its queue is APPEND-ONLY, so an item stays selectable
until acknowledged. Our sweeps read a MUTABLE set, and processing an item removes it from the
query that would re-derive the list. That is why "checkpoint a cursor, not a row per scan"
fails here and a frozen list is load bearing. The honest fix is not to substitute a cursor for
the page row. It is to make what the sweep reads append-only, at which point the page tables go
for Temporal's reason rather than by a substitution that converts at most once into at least
once.

**Temporal also lacks things this journal carries**, which is the other half of the comparison
and the reason table count alone is a poor score. Server-orchestrated compensation, topic
broadcast with subscriber fan-out, and payload offload with a collection lifecycle have no
counterpart there. Cascade cancellation exists but runs inside the history service with no
durable obligation row, so it does not survive that service dying mid-cascade. Any claim that
this system spends more state for less capability has to account for those first.

**Resonate keeps no outbox.** The header of its only migration, in
`crates/resonate-server-postgres/migrations/0001_initial.sql`, states the principle:

> There is no outbox either. A message is returned by the transition that emitted it and
> delivered by the caller, so there is nothing to store and nothing to drain.

Its timer and queue tables are partial indexes on the entity table rather than tables, its
fan-out edges are array columns with a GIN index rather than join tables, and its invariants are
named CHECK constraints named identically to properties in a machine-checked specification.

---

## The design

Five techniques, each of which a system in this class already ships without the tables we spend.
Two of the first four survived the per-table pass below unchanged. Two did not, and are written
here with the objection attached rather than removed, because the objection is the useful part.
The fifth came out of the comparison rather than the pass, and is the one with the most code
behind it.

**1. A transition returns its messages; the caller delivers them.** This removes the network hop
and the receipt matching: nothing is stored because nothing needs draining. It does NOT by
itself remove the delivery group, which is what this section originally claimed. The intent rows
carry a dedup identity that one transaction does not reproduce, and the group goes only if
something else carries it. See Open 3, which is where the real work is.

**2. Queues and timer sets become partial indexes on the entity they describe.** Membership of a
sweep is a predicate over the row, not a row of its own. This survives, with one boundary: it
applies to deciding WHO is due, not to a sweep already in flight, whose frozen item list is not
a predicate over current rows. The page tables below are that distinction.

**3. Small fan-out edges become array columns with a containment index.** The access pattern is
"who is waiting on me", which an array answers without a second table to keep consistent. This
does not work for `subscriptions`, the table it was aimed at. Each edge carries an allocated
`sequence` that `recipients::select` pages by, ordered and bounded by the broadcast's cutoff,
with a unique index that is also the only guard against a lost update in `signals::subscribe`.
An array of run ids on the topic carries no per-edge sequence and no index over it. Either the
technique is wrong here or fan-out paging has to change, and this proposal has not made that
case.

**4. Invariants become named CHECK constraints.** A state machine encoded structurally across
tables is a state machine nobody can read. A constraint that names the property it protects
fails with that property's name. The obstacle is dialect parity: `step_child_linkage` is the only
CHECK the journal declares and it exists only in the PostgreSQL artifact, so today this technique
has no enforcement at all on the SQLite tier. That is Open 1, and it is a precondition for this
technique rather than a detail under it.

**5. One discriminated mechanism instead of per-kind machinery.** This is the largest remaining
structural change and it was not in the first version of this proposal. `fanout` and
`propagation` are two implementations of one idea: take an obligation, page through the entities
it reaches, record what each page did, and refuse to run a page out of order. Each has its own
publication table, its own page table, its own history module and its own receipt validation.
Temporal's position is that a child workflow and an external signal are not special cases, and
the same argument applies here with more force, because our two cases are nearer to each other
than those two are. Collapsing them removes more code than deleting any table on the list above,
and unlike the deletions it does not require the relocation first.

---

## What is deleted

This list is the answer to Open 2, worked per table against `schema.ts` and the code that
enforces each property rather than in aggregate. It is shorter than the list this proposal
started with, because the per-table pass refuted most of it.

**Naming a home is not evidence that the home holds.** No table below is deleted on a claimed
invariant home that a mutation has not proven. For each deletion: neutralize the claimed OTHER
home, and require the test that binds the property to go RED there. A green under that mutation
means the claimed home does not bind the property, and the deletion would remove the last one
silently. A deletion lands with that evidence in its commit or it does not land.

This is not caution for its own sake. Every claim on this list is of one class - a reading about
where a property is bound - and that class was wrong three times in a single day, always in the
same direction, always understating what is bound. The audit reported nothing binds the frozen
scan plan; `tests/fanout/ordering.rs` turned out to bind the resume-at-a-different-page-size
half; `collection/rollback.rs` turned out to bind part of the frozen plan itself. Each was found
by looking, none by the previous reading.

One refinement, or the standard fails open. A mutation that leaves the suite green does not
distinguish "the claimed home does not bind this" from "a second guard is also refusing". Both
read as a quiet pass, and that is not hypothetical: `expect_err` passing over a REMOVED fence
because a second guard still refused is one of the things that bit the decoupling branch today.
So enumerate every enforcement point of the property BEFORE mutating, and mutate each arm. One
mutation cannot refute a disjunction.

**Deletable, with the invariant named.**

```
  outbox                      nothing reads it
  fanout_pages,
  propagation_pages           the frozen page result moves with its receipt
  subscriptions               an edge on the thing waited on, carrying its sequence
```

**The per-kind publication tables are no longer on this list.** They were, conditionally -
"only if the job id carries the identifying tuple" - and that condition was built and refuted.
A derived id cannot carry recording-time ordering, which the reconciliation sweep's captured
upper boundary is constructed on; Open 3 has the detail and the evidence. So
`advance_publications`, `fanout_publications` and `propagation_publications` MOVE at best, by
constraining the queue row on the same columns, and moving a constraint is not a reduction. They
are listed here as a deletion nowhere any more, and the honest shortening of this proposal is
that the largest single deletion it claimed is gone.

`outbox` is deletable for the opposite of the reason first given here. It is not the
transactional outbox; `job_publications` and the three per-kind tables are. `outbox` is an
application event log written by `emit` in `crates/zeroship-workflow/src/service/app.rs` and read
by nothing, its `delivered_at` declared in `models/schema_definition.rs` and never written or
read. Tests in `tests/management.rs` and `management/atomic_application.rs` use it as a rollback
witness and need a different one. The confusion is in the code as well as in this document:
`delivery.rs` and `runner/delivery.rs` both say "the creator outbox" in comments that are about
the publication intents, so the name already refers to two different things.

The per-kind tables go only under Open 3's second shape. Under the first they move rather than
disappear.

`subscriptions` can become an edge, but the edge has to keep a per-app allocated `sequence` with
a unique index over `(app_id, sequence)`. `recipients::select` pages by `sequence > cursor AND
sequence <= cutoff_sequence` ordered ascending, so the index is what makes that keyset safe, and
it is also the only thing that would catch a lost update in `signals::subscribe`, whose write of
`subscription_sequence` carries no compare-and-set guard.

**Not deletable. Each of these carries a property with no other home.**

```
  propagations                the cascade fence
  topics                      a revocation epoch and a sequence allocator
  collection_pages,
  reconciliation_pages        a frozen item list, which a cursor cannot represent
```

`propagations` answers a question no other table can. `propagation::fenced` treats every
cascading child of a parent generation as cancelled while that generation's cascade obligation is
unfinished, even though the child's own `control` column still reads `none`, and
`service/tests/propagation/fence.rs` binds exactly that. It is read from `frontier.rs`,
`delivery.rs::heartbeat_job`, `tasks.rs::heartbeat_inner` and `control/restart.rs`. Without the
row, in the window between a parent settling and the cascade page arriving, a leased child keeps
running and a child completing with `ContinueAsNew` starts a generation outside the cancellation.
This proposal filed it under delivery, which is the error: it is a liveness boundary between a
cancelled parent and its running children, and atomicity says nothing about it.

`topics` is not a subscription table. `signal_epoch` is the per-topic revocation epoch for signal
capability tokens, checked for equality at redemption so that a bump refuses every token already
minted, and `broadcasts`, which this proposal keeps, has a foreign key into it.
`completed_sequence` is the predecessor gate that holds a later broadcast unacknowledged until
its predecessor finishes; `tests/fanout/ordering.rs` settles that it cannot be replaced by
timestamps, backdating a broadcast's `created_at` and still requiring the earlier one first.

The page tables hold a frozen list, not a position in a derivable one. `prepare_collection` and
`prepare_reconciliation` read `plan` back on retry and never re-derive it, and the pending arm
ignores the retrying attempt's `page_size`. It has to work that way: processing an item removes
it from the query that would re-derive the list, since a purged payload leaves the state filter,
a deleted one has its `expires_at` pushed past the frozen cutoff, a confirmed publication leaves
the unconfirmed filter and a settled hold leaves `acquiring`/`releasing`. So an index into a
re-derived list addresses the wrong item.

Today that misalignment is caught rather than acted on, and the catch is itself made of the thing
this section is about. `next_collection_item` compares the plan `prepare_collection` returned
against the stored one, `if plan != *expected { return Err(invalid()) }`, and then selects by
`plan.ids.get(next)` from the STORED plan. A mutation that re-derives on retry therefore produces
a loud refusal, not a silent skip - measured, both dialects, not inferred. But the comparison is
only possible because a frozen list exists to compare against. Delete the row and there is no
`*expected`: the same drift that refuses today becomes an index into a shorter list, and the
sweep passes over the items in between without an error. The reduction would remove the detector
in the same change as the thing it detects. Their semantics
is at most once per item per page, from `delivery::reserve`'s compare-and-set committed before
the item's I/O; replacing the row with a cursor over item identity turns that into at least once.
`reconciliation_pages` also holds the captured upper boundary of a sweep's first page, which
`app_state` does not record until settlement.

**Unsettled.** `continuation_heads` and `continuation_members` were not resolved. The chain is
what lets a parent that accepted one generation of a child observe a later one, through
`read::resolve` following `steps.child_member_id` to the head and on to the current run, and
`tests/continuations.rs::pending_targets` binds it. "A continuation is a fresh root" deletes that
resolution rather than relocating it, which is a design decision this proposal has not made. Two
further facts belong with it: `steps`, a kept table, holds foreign keys into
`continuation_members`, and the `step_child_linkage` CHECK that ties a child checkpoint to its
accepted member exists only in the PostgreSQL artifact.

---

## What is kept, and why it is kept

**Compensation stays, including its durable retry state.** The columns on
`__zeroship_workflow_steps` that carry compensation attempts, timing and error are the one place
in this system where extra state buys something the field does not otherwise have. Every
comparable engine runs compensation as an in-code stack: a list of closures unwound in a
`catch`. That cannot survive the process dying mid-unwind, and the two hardest cases, a
compensation that itself fails and a rollback interrupted partway, are undocumented across the
systems that take that approach. Server-orchestrated compensation answers both. It costs
columns, not tables.

**The authority fence stays.** Refusing an operation whose authority has lapsed is not machinery
to be simplified away; it is the property the machinery exists for. Any reduction has to say
where each invariant lands, not merely which table disappears. This is a different fence from
the cascade fence on `propagations` above, which is about a cancelled parent's running children;
the two share a word and nothing else.

**Single writer per execution stays**, for the same reason.

---

## The lease, which is an invariant rather than a redesign

Both halves are already present. The missing piece is a rule keeping them apart:

> The absolute integer is the only representation that persists or crosses a process boundary.
> An `Instant` is derived at the enforcement site, and never travels, never persists, and is
> never compared against another process's.

An in-process timer is then explicitly not authoritative. It may fire late; it must never fire
early on something not due, and the durable sweep is what makes it safe for it to be wrong in
that one direction.

---

## What this is not

**It is not a rewrite.** The service is not carrying Temporal's complexity, and the evidence
does not support starting over. It is carrying a schema disproportionate to the engine around
it, and the disproportion is concentrated in one group with one cause.

**It is not an argument to copy Resonate.** That server runs as a single instance by its own
operational documentation, has no code versioning mechanism against a replay-from-the-top model,
and has no public production record. The techniques transfer. The architecture does not.

---

## Open

0. **Is reduction even the right work?** Asked first because it outranks the rest. This journal
   has no synchronous query of a running workflow's state and no visibility or search surface,
   and the comparable systems treat both as core. On a platform where agents build the apps,
   "what is this run doing right now" is a question that will be asked constantly, and today the
   only answer is reading journal rows directly. Adding that probably delivers more than every
   deletion below put together. This proposal should not be read as arguing otherwise; it argues
   only that if the state is reduced, these are the reductions that hold up.

1. **Does the SQLite dev tier take the same shape?** It should, and pre-launch there is no
   reason it cannot. Worth settling before the first table is deleted rather than after, because
   a second shape is how the delivery group came back last time.

2. **Which invariant does each deleted table carry today?** Answered, per table, in "What is
   deleted". The pass was worth more than the list it produced: it refuted most of the original
   list, and the tables it saved were saved by properties their names do not suggest. It left
   `continuation_heads` and `continuation_members` unsettled, which is a design call rather than
   a missing fact.

   It also surfaced three gaps that belong to the current code rather than to this proposal, and
   that anyone touching these tables should close first, stated with the boundary a mutation
   established rather than the one a reading claimed. The frozen item list IS partially bound
   today, by exactly one test: `collection/rollback.rs`'s receipt-failure body, which retries an
   aborted page and requires it to complete. Neutralizing the stored plan so the retry re-derives
   it reddens that body on BOTH dialects, and a solo re-run reproduced it, so the binding is real
   and not a starved deadline. Under the same mutation every other collection neighbour passes.
   What rollback does NOT bind is a retry at a DIFFERENT `page_size`, because it retries at the
   size it started with, and no collection or reconciliation test does. Nor does any of them
   abort the reserve transaction itself rather than the finalization that follows it. Fan-out is the counterexample worth copying rather than a gap:
   `tests/fanout/ordering.rs` delivers a page at size one, retries the unsettled page at a
   larger size, and asserts both the identical receipt and an unchanged snapshot, so the larger
   page delivered nothing more. The same body also binds replay duplication by counting a
   subscriber's signal rows, and carries a foreign app asserted at zero as its cross-tenant
   control. The collection path needs what fan-out already has.
   `subscription_sequence_unique` has no stated purpose anywhere, and the schema comment that
   appears to justify it describes standing in for a foreign key's supporting index, which does
   not apply because nothing references `subscriptions`. And `ingress::target_epoch` states in a
   comment that its callers hold the app lock through commit; all three do, but nothing in the
   signature enforces it, so a fourth caller would reintroduce the race the comment says cannot
   happen. The same shape appears on `closed_epoch` in `app.rs` and `deliver` in `signals.rs`.

3. **Where does the identifying tuple live once the per-kind tables are gone?** The easy version
   was rejected first: the tuple cannot simply go, because the job id is minted and the tuple is
   the only identity a publishable job has. That left two shapes - constrain the queue row on the
   same columns, or derive the job id so the primary key carries the identity and
   select-then-insert becomes an insert that collides. The second was the attractive one, because
   it lets the per-kind tables GO rather than move.

   **It was built, and it is refuted. A derived id costs a property nothing else provides.**

   The reconciliation sweep's publications phase captures an upper boundary as the greatest
   pending intent id, pages `after < id <= upper`, and rotates to the deployment-holds phase only
   when the page reports no more. That the boundary holds is CONSTRUCTED, not probabilistic:
   every intent writer and the capture take the same `app_state` row lock, so "recorded after
   capture" implies "minted after capture", and a minted `JobId` is a UUIDv7, so that implies
   "greater id". An intent written after the capture therefore sorts outside the window by
   construction, and the phase is guaranteed its turn.

   A derived id is a pure function of the work a job names. The work does not know when its row
   was recorded. **So no derived-id design can be ordered by recording time**, and this is not a
   matter of choosing better fields to hash.

   Two tests name the property in their own assertion messages: "newer intent is outside the
   captured upper boundary" in `tests/reconciliation.rs`, and "new publication must wait while
   holds receive their turn" in `tests/reconciliation/deployment_holds.rs`.

   The evidence that this is the derived id and not the box: across two runs of the same binary
   at different loads, the FAILING SET MOVED - one dialect failed in the first run and passed in
   the second while another failed in both. Load does not do that. It is the signature of a
   nondeterministic ordering dependency, which is exactly what a hash-ordered id introduces where
   a time-ordered one used to be.

   The cost is fairness rather than termination. A cycle still ends, because the window only
   shrinks; what is lost is the guarantee that the phase releasing deployments gets its turn
   promptly, and the bound becomes probabilistic where it was structural.

   **What would restore it**, and the size of it is the reason this is still open rather than
   decided: a monotone per-app sequence on `job_publications`, allocated under the app lock, with
   the publications phase cursoring on that instead of on the id. That needs the column, an
   allocator on `app_state`, a numeric encoding for the shared `reconciliation_after_id` and
   `reconciliation_upper_id` TEXT columns - the holds phase stores a `deploy_id` in them and
   `Plan::validate` compares them as strings - and an audit that every intent writer genuinely
   holds the app lock, which `advance`'s doc comment asserts and nothing enforces. That last item
   is the same shape as `ingress::target_epoch`: a lock convention stated in prose, true today,
   unenforced by any signature.

---

## Do-not notes

**Do not delete a table before naming where its invariant lands.** The delivery group is
deduplication state that `publication.rs` says explicitly nothing may retire. Committing in one
transaction replaces the reason it exists; it does not automatically replace every guarantee it
provides. The advance intent is the worked example: it reads as a projection of the intent row,
and it is the dedup key.

**Do not put creator values in the platform schema.** This is a constraint on everything here,
not an open question. A platform schema is not an at-rest home for creator data, which is why
the sibling proposal sequences the payload promotion BEFORE any reader exists in
`workflow_manager` rather than beside it: a reader is what puts creator payload at rest there.
The same rule appears at the crypto boundary, where a platform binding addresses no database and
`encryption::encryption_database` refuses it rather than defaulting one, so an encrypted column
on a platform schema stops at that refusal. Treat the refusal as the rule restated, not as an
obstacle to route around.

The gate on this has a trap worth naming, because the gate is a test and the test can pass
without the property. `journal_payload_columns_are_a_closed_set` selects a column whose type is
json, jsonb or bytea, or whose NAME is one of a short list. Creator payload in a text column
under another name is invisible to it: `steps.record` serialising a `StepCheckpoint`,
`signals.payload`, `broadcasts.payload`, `generations.error`, and the schedule inputs inside
`deploys.manifest`. Emptying the set as the predicate is written today turns the assertion green
and leaves the property false. The predicate widens and the set empties in one change, or
neither happens.

**Do not let a grouping carry the argument.** Every table above was deleted by the sentence
written against its group, not against it. `propagations` was filed under delivery and inherited
"transitions commit their own effects", which is true of delivery and says nothing about a
cancellation fence; `outbox` was filed under delivery and kept by an argument about the
transactional outbox, which it is not part of. A group is a reading aid. When it appears in the
justification column it has become a claim about every row, and the rows that are misfiled are
exactly the ones no one re-reads.

**Do not let `Instant` back across a boundary.** It is the representation that made two replicas
disagree, and it is comfortable to use because it is what the standard library hands you.

**Do not treat compensation as machinery to reduce.** It is the one place the extra state is the
point.
