# SC-2: the SQLite actor protocol

**Date:** 2026-08-26

**Status:** DRAFT - required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`

**Gates:** the SQLite arm of **SC-1** (cancellation and rollback there assume an
interruptible actor, which today's is not), the SQLite half of the epoch/lease
step, and the parent proposal's acceptance criterion that dropping a
caller-side future **races** the actor command's completion - resolved by an
actor-owned terminal CAS, not by the unconditional "drop cancels and rolls back"
an earlier draft of both documents asserted.

---

## Why this is a document and not a discovery

Two decisions here change **documented, user-visible** behaviour, so a black-box
suite cannot be trusted to settle them - it would pass on either answer:

1. whether an app's autocommit work stalls behind that app's own open creator
   transaction;
2. whether cancellation interrupts an in-flight statement or only takes effect
   between statements.

Both are recorded divergences today, not accidents, which is exactly why
changing them needs a decision rather than a patch.

## What today's actor actually is, verified

Three facts, each read rather than assumed:

- **Commands carry SQL and a reply channel, and nothing else.** There is no
  reservation, no owner, no cancellation token
  (`crates/zeroship-plugin-db/src/backend/sqlite/session.rs:162-172`).
- **Dropping the caller-side future does not cancel anything**, and the source
  says so in its own words (`session.rs:155-161`):

  > Sending on a dropped reply channel is a no-op - **the SQL has already
  > committed (or rolled back) by then**, so the only externally observable
  > consequence is the missed completion notification.

  So the parent proposal's acceptance criterion "dropping a caller-side future
  cancels the SQLite actor command and rolls back its transaction" is **false of
  today's actor by its own documentation**, not merely unimplemented.
- **One FIFO loop drives every route on one connection** -
  `while let Ok(cmd) = rx.recv()` on a dedicated OS thread (`session.rs:408`).

The consequence is already a recorded divergence
(`crates/zeroship-plugin-db/src/tx_route.rs:119-124`):

> SQLite runs the whole app on one connection ... so a correctly pool-routed
> write still executes inside whatever transaction that connection is holding.
> ... Measured and recorded in `docs/reference/sqlite-divergences.md`.

## Decision 1 - connection model: two per attached app

The actor owns, per attached app file:

- **`tx_conn`** - reserved to at most one explicit creator transaction;
- **`op_conn`** - autocommit operations, each wrapped in
  `BEGIN DEFERRED ... COMMIT`.

WAL permits this concurrency; one connection cannot. This **retires** the
`tx_route.rs:119-124` divergence rather than documenting it further: an app's
autocommit reads stop executing inside that app's open creator transaction, and
`docs/reference/sqlite-divergences.md` loses that entry in the same change.

It is also what SC-1 needs on this backend. SC-1 admits one top-level
transaction per `(runtime_instance_id, app_id)`; with a single shared
connection, that admission would additionally serialise every unrelated
autocommit operation behind it, which is a different and stricter promise than
the PostgreSQL arm makes.

## Decision 2 - cancellation interrupts in-flight statements

`rusqlite` 0.39.0 - the version `Cargo.lock` resolves - exposes
`Connection::get_interrupt_handle()` (`src/lib.rs:1018`) returning an
`InterruptHandle` (`:1269`) with `interrupt()` (`:1279`). The handle is the
mechanism; the contract is:

- the caller-side guard holds a cancel handle; **its drop signals the actor**
  and calls `interrupt()` on the reservation's connection;
- the actor, on observing the interrupt, **rolls that connection back, retires
  the reservation, and only then acknowledges**;
- transaction state lives in the actor, so a caller drop can never release state
  the actor is still using.

Two details that are contract, not implementation, because getting either wrong
turns a deliberate cancellation into a mystery:

- **"Rolls that connection back" covers two different server states.** An
  interrupted *write* may already have been rolled back by SQLite itself, while
  an interrupted read leaves the transaction open. The actor must reach the same
  end state from both, and must not treat "rollback returned an error because
  there was no transaction" as a failure.
- **`SQLITE_INTERRUPT` needs an arm in the error mapper, and has none today** -
  the string appears **zero** times anywhere in `crates/` or `libs/`. Without
  one, a cancellation the platform *asked for* surfaces as an unmapped, opaque
  database error, which is indistinguishable from a real fault at exactly the
  moment an operator is trying to understand a timeout. The arm maps it to the
  cancellation outcome the caller already expects.

**Threading model:** one actor thread per attached app file, owning both of that
app's connections, rather than one loop per connection. Two loops would need
their own coordination to keep a reservation's commands ordered, which is the
problem the reservation exists to solve. What "unblocked" means is therefore
precise: commands for the *other* reservation are dispatched between commands of
the first, not concurrently with a single command.

Cancellation therefore takes effect **during** a long statement, not only in the
gaps between statements. That is the stricter of the two available answers and
the one SC-1's deadline rule needs: a deadline that cannot interrupt a running
statement is not a deadline.

## The reservation protocol

```text
Reserve(kind) -> ReservationId          kind = Autocommit | Transaction
<command>(ReservationId, ...)           every command names its reservation
AuthorityRead(app_id) -> AuthorityRow   epoch + incarnation + ceiling; op_conn only
Settle(ReservationId, outcome)          explicit transactions only
Release(ReservationId)                  autocommit, at command completion
Cancel(ReservationId)                   explicit; a caller-side drop is NOT observable
DetachApp(app_id, expected_incarnation)  restore/teardown
```

**`AuthorityRead` exists because SC-6 assigns the dev tier's mid-transaction
authority read to `op_conn`, and this protocol had no command that could carry
it** - `ceiling` appeared nowhere in this document while SC-6 depended on it.
Each document was complete on its own; the gap existed only in composition,
which is why neither document's own review could see it.

Two properties make it a distinct command rather than an ordinary one:

- **It names no reservation, and deliberately cannot run on the transaction's.**
  A transaction's reservation holds a deferred WAL snapshot fixed at its first
  statement, so a read issued through it **structurally cannot observe a ceiling
  lowered after `BEGIN`** - the same stable-snapshot property this document
  relies on elsewhere. Routing authority through it would satisfy the letter of
  "read it per authorization" while guaranteeing the stale answer, and the
  resulting test would pass.
- **It is the SQLite counterpart of the separate platform-role session** the
  PostgreSQL tier uses. The tiers differ in mechanism and agree in contract: an
  authority read never rides the data snapshot.

- The actor **rejects a command whose reservation does not match** the
  connection's current owner, with a typed error rather than silently running it
  on whatever connection is free. That mismatch is the class of bug the current
  shape cannot even express.
- Autocommit reservations settle at command completion; transaction reservations
  at `Settle` or cancellation.
- **Unrelated autocommit READS are not blocked** by a transaction reservation on
  the other connection - which is the whole point of Decision 1. Not "queued
  work", which an earlier draft said here and which over-promises in the one
  direction SQLite cannot deliver: a *write* still waits on the single-writer
  lock however many connections exist. The acceptance arm below states the same
  limit, and the two must not drift apart again.
- `DetachApp` settles or aborts outstanding reservations and closes both
  connections **before** restore's file swap, so a lock release cannot leave a
  connection bound to an obsolete inode.

## The SQLite epoch

The authority row is
`__zeroship_state(state, epoch, incarnation, deprovisioned_at, ceiling,
changed_at)` **in the app
file**, written only by the migration path in the same transaction as its DDL
(SQLite DDL is transactional, and the adapter refuses non-transactional forms
anyway). Every reservation's first statement reads it.

It carries `incarnation`, `deprovisioned_at` and `ceiling` because
`AuthorityRead` above promises all three, and an earlier draft listed only
`(state, epoch, changed_at)` - the command would have had nothing to return.
There is no authority domain on this tier: `(system_identifier, timeline_id)`
identifies a PostgreSQL cluster and its recovery timeline, and a local file has
neither. **The dev tier therefore has no PITR-resurrection defence**, which is
the same honest split SC-6 already draws for the ceiling: the developer owns
the bytes, and no scheme in the file can change that.

This is the SQLite counterpart to the parent proposal's `__zeroship_admin`
placement, and the reason it differs: there is no second, platform-owned
database to put it in - the app file is the whole world. The tenant-cannot-write
property that motivates `__zeroship_admin` on PostgreSQL is provided differently
here, by the file being reachable only through this actor.

**Open, and deliberately not decided here:** which process writes the first
stable row on each dev path. The dev command today always derives SQLite paths
(`sdks/vite-plugin/src/cli/migrate-dev.ts:116-131`) and the addon exposes only
`applyIrSqlite`, so the PostgreSQL-dev-URL question is entangled with it. That
belongs to **SC-4**.

## Acceptance shape

- **Concurrency arm:** app A's autocommit **reads** proceed while A holds an
  open explicit transaction. This is the arm that proves Decision 1 landed; it
  fails on today's single connection.

  It says **reads** deliberately. WAL gives concurrent readers, not concurrent
  writers: an autocommit *write* still contends for the single write lock held
  by `tx_conn` and waits up to `busy_timeout` (5000 ms,
  `crates/zeroship-plugin-db/src/backend/sqlite/session.rs:369`). An earlier
  draft promised that "autocommit operations proceed", which would have been
  read as covering writes and is not achievable on any number of connections -
  SQLite has one writer per database. Claiming it would have set an arm that can
  never pass.
- **Interrupt arm:** cancellation takes effect *during* a long-running
  statement, not after it. This is the arm that proves Decision 2 landed; it
  fails on today's actor, whose own comment says the SQL has already completed.
- A command bearing a stale or foreign `ReservationId` is refused with a typed
  error.
- Dropping a caller-side future **races** the command's completion, and the
  actor resolves that race with a terminal CAS it alone owns:
  - **cancellation wins:** interrupt, roll the reservation's connection back,
    retire the reservation, and only then acknowledge;
  - **completion wins:** the outcome is `AlreadyCompleted`, and **no rollback is
    claimed**.

  An earlier draft required the drop to roll back *unconditionally*, and that
  cannot pass: the actor may commit and send its reply before the caller ever
  polls, and nothing dropped afterwards can un-commit it. Worse than
  unachievable, an unconditional promise invites a caller to treat a dropped
  future as proof the write did not land, which is the most dangerous thing this
  protocol could tell anyone. The protocol therefore needs an explicit
  `Cancel(ReservationId)` command rather than treating a drop as one, since a
  drop is not observable by the actor at all.
- `DetachApp` during an open transaction settles or aborts it and closes both
  connections before the file is replaced.
- A reservation's first statement reads the epoch row, and a migration
  committing DDL concurrently leaves that reservation **coherently old or
  coherently new, never torn**.

  Not "is observed as a changed epoch", which an earlier draft required and
  which is impossible for the schedule that matters: once the reservation's
  deferred WAL transaction has taken its snapshot with the first epoch `SELECT`,
  a migration committing afterwards is *by design* invisible to it - the parent
  proposal relies on exactly that stable snapshot. Demanding the new value
  contradicts the mechanism the same documents specify. What matters for
  correctness is coherence, not freshness: an old-snapshot reservation that then
  attempts a **write** fails on `SQLITE_BUSY_SNAPSHOT`, which is the real
  serialization point and needs its own mapped arm.
