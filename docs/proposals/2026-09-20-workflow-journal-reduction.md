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
  delivery         outbox, job_publications, advance_publications, fanout_publications,
                   propagation_publications, fanout_pages, propagations, propagation_pages
  bookkeeping      job_receipts, management_receipts, requests,
                   collection_pages, reconciliation_pages
  pub/sub          topics, broadcasts, subscriptions
  deployment       deploys, deployment_holds, activations, activation_scopes
  continuation     continuation_heads, continuation_members
  schedules        schedules, occurrences
  housekeeping     schema_version, app_state
```

**The delivery group is the largest, and it is there for one reason.**
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
creator should not be able to drop the log the platform is executing against. It is also the
largest simplification available, and the system's biggest group of tables is a direct
consequence of the split it closes.

---

## What comparable systems do

Both of these were read from source, not from summaries.

**Temporal keeps one outbox and discriminates.** Its per-category task tables are marked in
`common/persistence/sql/execution_tasks.go` as existing for backward compatibility; every
category added since goes into two generic tables keyed by a category id. Paging is not a table
at all: queue readers keep positions in memory and checkpoint an ack level into the shard
record. Child workflows and external signals are not special cases; both are an event, an entry
in a per-execution map, and one outbox row, written atomically.

**Resonate keeps no outbox.** The header of its only migration, in
`crates/resonate-server-postgres/migrations/0001_initial.sql`, states the principle:

> There is no outbox either. A message is returned by the transition that emitted it and
> delivered by the caller, so there is nothing to store and nothing to drain.

Its timer and queue tables are partial indexes on the entity table rather than tables, its
fan-out edges are array columns with a GIN index rather than join tables, and its invariants are
named CHECK constraints named identically to properties in a machine-checked specification.

---

## The design

Four techniques, each of which a system in this class already ships without the tables we spend.

**1. A transition returns its messages; the caller delivers them.** This removes the delivery
group outright rather than compressing it. Nothing is stored because nothing needs draining.

**2. Queues and timer sets become partial indexes on the entity they describe.** Membership of a
sweep is a predicate over the row, not a row of its own.

**3. Small fan-out edges become array columns with a containment index.** The access pattern is
"who is waiting on me", which an array answers without a second table to keep consistent.

**4. Invariants become named CHECK constraints.** A state machine encoded structurally across
tables is a state machine nobody can read. A constraint that names the property it protects
fails with that property's name.

---

## What is deleted

```
  the delivery group          transitions commit their own effects
  collection_pages,
  reconciliation_pages        resumable scans checkpoint a cursor, not a row per scan
  continuation_heads,
  continuation_members        a continuation is a fresh root, not a tracked chain
  topics, subscriptions       a waiter is an edge on the thing waited on
```

Each line is a claim that a capability survives the table. None of them is a claim that the
capability is unnecessary.

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

**The fence stays.** Refusing an operation whose authority has lapsed is not machinery to be
simplified away; it is the property the machinery exists for. Any reduction has to say where
each invariant lands, not merely which table disappears.

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

1. **Does the SQLite dev tier take the same shape?** It should, and pre-launch there is no
   reason it cannot. Worth settling before the first table is deleted rather than after, because
   a second shape is how the delivery group came back last time.

2. **Which invariant does each deleted table carry today?** This has to be answered per table
   before deletion, not in aggregate. A table that turns out to be the only writer of a
   uniqueness constraint is not a table whose capability survives its removal.

3. **Where does the identifying tuple live once the per-kind tables are gone?** Answered far
   enough to reject the easy version: it cannot simply go, because the job id is minted and the
   tuple is the only identity a publishable job has. Two shapes remain open. Constrain the queue
   row on the same columns, which keeps the constraint and moves it. Or derive the job id from
   `(kind, entity, revision, available_at)` so the primary key carries the identity and
   select-then-insert becomes an insert that collides. The second is what lets the per-kind tables
   go rather than move, and it is the one to cost out. `available_at` is part of the tuple today,
   so a derived id has to include it, and rescheduling a frontier to a new due time has to keep
   producing a new job.

---

## Do-not notes

**Do not delete a table before naming where its invariant lands.** The delivery group is
deduplication state that `publication.rs` says explicitly nothing may retire. Committing in one
transaction replaces the reason it exists; it does not automatically replace every guarantee it
provides. The advance intent is the worked example: it reads as a projection of the intent row,
and it is the dedup key.

**Do not let `Instant` back across a boundary.** It is the representation that made two replicas
disagree, and it is comfortable to use because it is what the standard library hands you.

**Do not treat compensation as machinery to reduce.** It is the one place the extra state is the
point.
