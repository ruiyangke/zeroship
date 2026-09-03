# SC-2: the SQLite actor protocol

**Date:** 2026-08-26

**Status:** implemented, except a production cancellation consumer - the
cancel surface exists and is exercised by tests, and SC-1's deadline rule owns
the wiring that will call it in production.

Required sub-contract of
`docs/proposals/2026-08-26-runtime-db-binding-design.md`. Read the set from
`docs/proposals/2026-08-26-runtime-db-binding-00-index.md`; live defects cited
by number live in
`docs/proposals/2026-08-26-runtime-db-binding-defect-register.md`.

---

## Scope

Two things this protocol fixes are **creator-visible** behaviour, not internal
mechanics, so each also has a row in `docs/reference/sqlite-divergences.md` and
the two must be kept in step:

1. whether an app's autocommit work executes inside that app's own open creator
   transaction;
2. whether cancellation interrupts an in-flight statement or only takes effect
   between statements.

The pre-SC-2 actor could express neither question. Commands carried SQL and a
reply channel and nothing else - no reservation, no owner, no cancellation
token - and one FIFO loop drove every route on one connection.

## Decision 1 - connection model: one shared autocommit connection, one transaction connection per app

The actor owns:

- **`op_conn`** - autocommit operations for **every** app, each wrapped in
  `BEGIN DEFERRED ... COMMIT` where the statement permits it;
- **one transaction connection per app that has opened a transaction** - each
  reserved to at most one explicit creator transaction, opened lazily on that
  app's first `db.transaction()`, ATTACHing only that app's file.

One connection cannot express the separation at all. This retires the
divergence formerly recorded at `tx_route.rs:119-124`: an app's autocommit reads
no longer execute inside that app's open creator transaction, and an autocommit
write is no longer destroyed by that transaction's `ROLLBACK`.

**Why the transaction half is per app and the autocommit half is not.** The
transaction connection carries state across commands - an open `BEGIN` - so a
shared one makes the admission key `(runtime_instance_id, session)` and refuses
app B's `db.transaction()` while app A holds one (defect L22b). An autocommit
reservation is minted per command and settles at that command's completion, so
`op_conn` has no cross-command state for two apps to share; splitting it would
buy scheduling parallelism a single actor thread cannot deliver anyway. Carrying
the lane id in `Lane::Tx(TxLaneId)` is what makes this protocol's admission key
the same key SC-1 states: one top-level transaction per
`(runtime_instance_id, app_id)`.

A transaction lane's connection ATTACHes one app's file and no other, so a
creator transaction cannot name another tenant's tables even if a SQL builder
were tricked into emitting one (`session.rs:1670-1700`).

**One loop owns every connection.** Per-connection loops would need their own
coordination to keep a reservation's commands ordered, which is the problem the
reservation exists to solve. So "unblocked" is precise: commands for the *other*
reservation are dispatched **between** commands of the first, not concurrently
with a single command.

**Bounded lanes, and two distinct refusals.** A session holds at most
`MAX_TX_LANES = 8` transaction connections (`session.rs:553`); an idle lane is
evicted to make room, and only a session where all eight are mid-transaction
refuses. The two exhaustion cases get different wire codes because the remedies
are opposite and only one of them is the creator's to act on:

| code | means | remedy |
| --- | --- | --- |
| `transaction_connection_busy` (`session.rs:727`) | *this* app already has a transaction open | finish it |
| `transaction_lanes_exhausted` (`session.rs:113`, raised at `:1678-1697`) | the dev process hosts more apps than it has lanes for; the creator's code is blameless | retry |

Collapsing both into one code is exactly defect L22b.

### Accepted costs

Both are deliberate. Do not "fix" either without reopening the decision.

- **Exhaustion refuses immediately; it does not queue.** A second
  `db.transaction()` for the same app is refused at once rather than waiting for
  the lane, because a refusal is decided from state the caller can see and needs
  no deadline to be safe. The PostgreSQL half queues on the pool's
  `acquire_timeout` instead, so the two tiers differ in what a creator observes
  under contention. When SC-1's deadline lands, whether this becomes a bounded
  wait is that decision's to make.
- **Cross-tenant head-of-line blocking is structural.** One FIFO actor loop
  means a long-running statement from one app stalls every other tenant's reads
  on the same session. A thread per app would remove it; the shipped shape does
  not. The judgement that this is acceptable rests on reachability: no shipped
  vector puts two apps in one SQLite process - the worker exits on a SQLite DSN
  (`zeroship-worker/src/main.rs:328-334`), `resolve_num_workers` clamps
  `zeroship serve` to one thread on a SQLite URL
  (`zeroship-runtime/src/core/serve.rs:171-179`), and `app_id` there comes from
  one process-wide `env_vars` map defaulting to `"default"`
  (`zeroship-runtime/src/core/plugin.rs:229-232`). The day a vector puts two
  apps in one process this becomes live; the connection map is already keyed by
  app, so promoting each entry to its own thread is a local change.

### What WAL does and does not buy: app files are not in WAL

`PRAGMA journal_mode` is per database and does **not** propagate across
`ATTACH`, and the migration engine pins every app file to DELETE and refuses to
proceed otherwise
(`crates/zeroship-migrate-sqlite/src/backend/actor.rs:719-729`):

```
PRAGMA "<schema>".journal_mode = DELETE
  ... "journal_mode remained {actual}; DELETE rollback journaling is
       required for atomic app+journal commits"
```

So the session's own database is WAL and every `zs-<app>.sqlite` is
**rollback-journal**. The consequences are load-bearing for anything reasoning
about concurrency here:

- Concurrent readers on app data come from rollback journalling's `SHARED` /
  `RESERVED` split, not from WAL snapshots. That is enough for the concurrency
  arm - a reader holds `SHARED` while the transaction connection holds
  `RESERVED`.
- There is **no per-connection snapshot on app data and no
  `SQLITE_BUSY_SNAPSHOT` there.** The same schedule that yields
  `SQLITE_BUSY_SNAPSHOT` (517) on `main` yields a plain `SQLITE_BUSY` on an
  attached app file. Any argument in this set that leans on a stable WAL
  snapshot applies to the session's own database only.
- An autocommit *write* still contends for SQLite's single writer lock and waits
  out `busy_timeout` (5000 ms, set by the boot PRAGMAs at `session.rs:1457-1459`)
  before reporting lock contention. No number of connections changes that; reads
  are unaffected.

`an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal`
(`crates/zeroship-plugin-db/tests/sqlite_integration.rs:10806`) pins the journal mode of both databases so
a change to that fact fails there rather than silently invalidating the
reasoning above. `docs/reference/sqlite-divergences.md` carries the
creator-facing form.

**Boot PRAGMA order is a trap.** Every connection runs
`journal_mode = WAL; synchronous = NORMAL; busy_timeout = 5000;
foreign_keys = ON` in that order, and `journal_mode` MUST come first:
`synchronous = NORMAL` is crash-safe in WAL and is not in rollback-journal mode.

## Decision 2 - cancellation interrupts in-flight statements

`rusqlite` 0.39.0 - the version `Cargo.lock` resolves - exposes
`Connection::get_interrupt_handle()` returning an `InterruptHandle` with
`interrupt()`. The handle is the mechanism; the contract is:

- the caller-side guard holds a cancel handle; **its drop signals the actor**
  and calls `interrupt()` on the reservation's connection;
- the actor, on observing the interrupt, **rolls that connection back, retires
  the reservation, and only then acknowledges**;
- transaction state lives in the actor, so a caller drop can never release state
  the actor is still using.

An interrupt is delivered only if the target lane's connection is still the
generation the caller aimed at (`Interrupts::interrupt`, `session.rs:379`). A
lane that was evicted, or never opened, reports "not delivered" for the same
reason a stale generation does: there is no connection this cancellation has any
right to touch.

Two details are contract, not implementation, because getting either wrong turns
a deliberate cancellation into a mystery:

- **"Rolls that connection back" covers two different server states.** An
  interrupted *write* may already have been rolled back by SQLite itself, while
  an interrupted read leaves the transaction open. The actor must reach the same
  end state from both, and must not treat "rollback returned an error because
  there was no transaction" as a failure - that case is classified by
  `is_autocommit`, per the table below.
- **`SQLITE_INTERRUPT` (extended code 9) has its own arm in the error mapper**
  (`zeroship-data-sqlite/src/error.rs:48,69-81`). Without one, a cancellation the platform
  *asked for* surfaces as an unmapped, opaque database error, indistinguishable
  from a real fault at exactly the moment an operator is trying to understand a
  timeout.

Cancellation therefore takes effect **during** a long statement, not only in the
gaps between statements. That is the stricter of the two available answers and
the one SC-1's deadline rule needs: a deadline that cannot interrupt a running
statement is not a deadline.

**No production canceller exists yet.** `SqliteCancelHandle`,
`SqliteCancelGuard` and `Interrupts::interrupt` carry `#[allow(dead_code)]`, and
that annotation is real rather than defensive: SC-2 owns the primitives, SC-1
step 9 owns the deadline and dropped-future wiring that calls them, and until it
lands the only callers are this crate's tests.

### The four interleavings, and the definition that makes them decidable

"Cancellation interrupts in-flight statements" is only implementable if
"in-flight" is something the platform can **observe**. It cannot observe when
SQLite reaches its first `sqlite3_step`. So the boundary is defined at a state
we own:

> **Execution start is the actor's transition to `Running`** - a non-zero
> `Reservation::running_seq` - not an unknowable first `sqlite3_step`.

The handshake between a cancelling caller and the executing actor is Dekker's,
on `SeqCst` (`zeroship-data-sqlite/src/reservation.rs:19-38`):

```text
caller: terminal.store(CancelIntent)  ; then load(running_seq)
actor : running_seq.store(seq)        ; then load(terminal)
```

`SeqCst` gives a single total order over those four operations, so at least one
side observes the other, whichever thread was scheduled first. That is what
turns four fuzzy races into four decidable cases:

1. **Cancel before execution starts.** The caller sets the cancel intent in the
   shared terminal word, then enqueues `Cancel`. The actor sees the intent in
   its **pre-start** check and claims the terminal as a cancellation. If nothing
   began, it removes the queued operation, retires the reservation and
   acknowledges `Cancelled(NoSqlStarted)` - **no `BEGIN`, no data SQL, ever
   issued** - and SC-1 releases its admission only after reducing that proof. If
   `BEGIN` had already succeeded, it rolls back first.
2. **Cancel during a long statement.** The caller marks the exact target -
   reservation, lane, connection generation, command sequence - and calls
   `interrupt()` on **that** target's handle. The lane matters: autocommit work
   targets `op_conn`, a transaction's data and terminal SQL target its own lane.
   The direct interrupt stops an already-running statement; the progress latch
   (`reservation.rs:138`, `:256`) covers the narrow window where the actor has
   stored `Running` but SQLite has not yet stepped, which is precisely the gap
   the raw interrupt is documented to no-op in.
3. **Cancel after commit, before the reply is polled.** The actor claims the
   terminal as a completion *before* sending `COMMIT`, stores the outcome, and
   moves to terminal **before** sending the reply. A drop of the unpolled future
   therefore observes a promised completion, **sets no cancel intent and does
   not call `interrupt()`** - it joins as a terminal waiter and receives
   `AlreadyCompleted` carrying that exact stored outcome. No `ROLLBACK` is sent;
   **the write stays durable.**
4. **Cancel after the reply is polled.** The future disarms its drop-cancel
   guard **before** returning `Ready` (`SqliteCancelGuard::disarm`), so a later
   drop cannot retroactively cancel a delivered result. The poll order closes the
   registration race: fast-path the reply, register the real waker on `Pending`,
   then re-poll the reply once more before sleeping.

Cases 3 and 4 are the ones worth stating explicitly, because both are places
where a plausible implementation destroys committed data in the name of honoring
a cancellation. **A cancellation that arrives after the outcome is decided is a
question, not a command.**

A `Drop`-fired cancellation short-circuits on *both* terminal verdicts:
`AlreadyCompleted` and `AlreadyCancelling`. Nobody awaits a guard's answer, so a
`Cancel` it queues can only *act*, and queuing a second one for a reservation
another caller is already cancelling asks the actor to run cleanup twice on a
shared connection.

### The terminal classifier: `is_autocommit` is the authority, never the result code

**A result code alone never decides a transaction's fate.** `COMMIT` returning
`Err` does not mean the transaction rolled back - SQLite may have
auto-rolled-back, or the failure may have arrived after the commit was durable.
Guessing either way is wrong; one guess reports a loss that did not happen, the
other reports a success that did not.

So the contract is: after any terminal statement, the actor finalizes the
statement, proves `!is_busy()`, and then **samples `is_autocommit`**. That
sample, not the error, classifies the outcome (`classify_commit` /
`classify_rollback`, `zeroship-data-sqlite/src/reservation.rs:519-620`):

| terminal statement | raw result | `is_autocommit` after | outcome |
| --- | --- | --- | --- |
| `COMMIT` | `Ok` | true | **Committed.** The only confirmed-commit arm. |
| `COMMIT` | `Ok` | false | Protocol contradiction. Quarantine, return `CommitIndeterminate`. **Never publish success.** |
| `COMMIT` | `Err` (incl. `SQLITE_INTERRUPT`/`BUSY`/`LOCKED`) | false | Commit did not finish. Issue one `ROLLBACK`; if it then reads true, `CommitFailed`; if still false, quarantine as `CommitIndeterminate`. |
| `COMMIT` | `Err` (incl. `SQLITE_INTERRUPT`) | true | The transaction ended, but **nothing here proves commit versus auto-rollback**. `CommitIndeterminate`, quarantine. No SQLite code alone upgrades this. |
| `ROLLBACK` (explicit) | `Ok` | true | `RolledBack`. |
| `ROLLBACK` (cancellation) | `Ok` | true | `Cancelled`, cleanup `RolledBack`, original cause latched. |
| `ROLLBACK` | `Err` | true | SQLite already ended it. Explicit -> `RolledBack`; cancellation -> `Cancelled(AlreadyRolledBack)`. Error retained as diagnostics only. |
| `ROLLBACK` | either | false | Cleanup unproved. Quarantine: `RollbackFailed` / `CleanupIndeterminate`. |

Three consequences, each a place the obvious implementation goes wrong:

1. **`CommitIndeterminate` is a real terminal outcome, not an error.** The
   temptation is to collapse it into failure. That is the same mistake as
   **defect L8**, where PostgreSQL answers a failed `COMMIT` with a `ROLLBACK`
   tag and the plugin reported success: in both cases the true outcome is not
   what the naive read of the reply says, and the fix is to represent the
   uncertainty rather than resolve it by assumption.
2. **The raw `rusqlite` error survives until the actor decides.** Mapping it
   inside the exec/query bodies erases the reservation, ownership and
   cancellation-intent context the classifier needs, so the mapping happens at
   the actor's terminal step.
3. **An autocommit operation error is never followed by a `COMMIT`.** It is
   terminalized first, and the completing owner either observes `is_autocommit`
   already true or drives the `ROLLBACK` rows above - so a constraint violation,
   an interrupt, or an I/O error can never be accidentally committed.

## The reservation protocol

Every data command names its reservation, and the actor refuses one whose
reservation does not own the connection it would run on rather than running it
on whatever connection is free. That mismatch is the class of bug the pre-SC-2
shape could not express.

```text
Reserve(reservation, app_id)     bind this app's tx connection; opens the lane
Release(reservation)             retire a tx reservation, rolling back leftovers
Exec / Query / QueryTyped        data commands, each naming its reservation
Settle(reservation, intent)      COMMIT | ROLLBACK, classified as above
Cancel(reservation)              explicit; a caller-side drop is NOT observable
Attach(app_id, db_path)          ATTACH on op_conn and record for replay
VacuumInto(app_id?, dest_path)   snapshot on op_conn
ReattachFile(app_id, temp, live) restore by atomic file swap
Shutdown                         drain and exit
```

Rules that govern the set:

- Autocommit reservations are minted per command and settle at that command's
  completion; transaction reservations settle at `Settle` or cancellation. A
  spent autocommit reservation is refused as a non-owner.
- `Reserve` rolls back anything the previous binding left open before rebinding,
  so a lease dropped without settling cannot leak its transaction into the next
  one - and a `Release` lost to a full queue cannot strand the lane.
- **Unrelated autocommit READS are not blocked** by a transaction reservation on
  another connection, which is the whole point of Decision 1. Not "queued work":
  a *write* still waits on the single-writer lock however many connections
  exist.
- `ReattachFile` DETACHes the app's alias on `op_conn` and on that app's
  transaction lane if one is open, renames, and re-ATTACHes on both
  (`session.rs:2602-2694`). Both are detached **before** the rename because a
  lock release must not leave either connection bound to an obsolete inode. If
  the first DETACH fails nothing is renamed; if the second fails, the first
  connection's alias is restored.

  **It must also settle or abort outstanding reservations first.** A DETACH
  issued while a reservation is mid-flight leaves that reservation holding a
  connection whose alias has moved, which no terminal classifier row describes.
  This is a requirement SC-5 states and this document owes: SC-5's PostgreSQL
  arm is scoped to PostgreSQL *because* of it - "a deprovision does not abort an
  in-flight operation" and this rule are **jointly unsatisfiable on SQLite**, so
  an unscoped no-abort rule would contradict the restore path. Graceful
  deprovision and forced file detach are different events; only the first is
  covered by SC-5's arm. **POSIX `rename` is atomic only within one
  filesystem**, so operator snapshot destinations must live on the same FS as
  the live per-app file - a cross-FS destination cannot complete an atomic
  restore.

## No database-resident authority row on this tier

There is no `__zeroship_state` row, no epoch, and no `AuthorityRead` command.
The runtime descriptor is the sole authority for schema, and on the SQLite tier
it always effectively was; `ceiling` is worker configuration, a field of the
binding, not a column an app file carries. The argument, column by column, is
`docs/reviews/2026-08-28-sqlite-authority-row.md`.

Two facts that follow, and that anyone tempted to reintroduce a row should
weigh:

- The hazard a file-resident epoch would supposedly cover - an app file swapped
  beneath a live connection - is precisely what such a row **cannot** see: the
  connection stays on the old inode and reads the old row. `ReattachFile` is the
  answer, and it takes no incarnation and reads no row.
- A deferred read transaction anchors its snapshot at its first read, whatever
  that read is. Making the first read a `SELECT` against an authority row moves
  the anchor microseconds earlier and changes nothing else.

There is also no authority-domain equivalent here: `(system_identifier,
timeline_id)` identifies a PostgreSQL cluster and its recovery timeline, and a
local file has neither. **The dev tier therefore has no PITR-resurrection
defence**, which is the same honest split SC-6 draws for the ceiling - the
developer owns the bytes, and no scheme inside the file changes that.

## Acceptance shape

Each arm below has a test, in `crates/zeroship-plugin-db/tests/sqlite_integration.rs`
unless noted.

| arm | test |
| --- | --- |
| App A's autocommit **reads** proceed while A holds an open transaction, and do not observe its uncommitted write | `an_autocommit_read_proceeds_while_the_app_holds_an_open_transaction` (`:9956`) |
| Two apps hold transactions at the same time | `two_apps_hold_transactions_at_the_same_time` (`:10470`) |
| A transaction lane cannot address another app's tables | `a_transaction_lane_cannot_address_another_apps_tables` (`:10545`) |
| A second transaction *for the same app* is refused, and the message names the app | `a_second_transaction_for_the_same_app_is_still_refused_and_names_it` (`:10597`) |
| Lane exhaustion is capped and refused under its own code | `transaction_lanes_are_capped_and_the_refusal_has_its_own_code` (`:10646`) |
| A command bearing a stale or foreign `ReservationId` is refused with a typed error | `a_command_bearing_a_foreign_reservation_is_refused` (`:10004`), `a_spent_autocommit_reservation_is_refused_as_a_non_owner` (`:10338`) |
| Cancellation takes effect *during* a long-running statement | `a_cancellation_interrupts_a_statement_that_is_already_running` (`:10061`) |
| A cancellation after commit does not roll the commit back | `a_cancellation_after_commit_does_not_roll_the_commit_back` (`:10129`) |
| A cancel for a retired reservation does not touch the next transaction | `a_cancel_for_a_retired_reservation_does_not_roll_back_the_next_transaction` (`:10194`) |
| A second cancellation is answered, not re-executed | `a_second_cancellation_is_answered_not_re_executed` (`:10290`) |
| All eight classifier rows, with a fault injected at the terminal statement and `is_autocommit` sampled after | unit tests in `src/zeroship-data-sqlite/src/reservation.rs` (`:625+`) |
| `SQLITE_INTERRUPT` maps to the cancellation code and does not fall through to the catch-all | `src/zeroship-data-sqlite/src/error.rs:288-303` |
| A write upgrade on a stale WAL snapshot is refused with `SQLITE_BUSY_SNAPSHOT` | `a_write_upgrade_on_a_stale_wal_snapshot_is_refused` (`:10718`) |
| App files are DELETE, so the same schedule there is a plain `SQLITE_BUSY` | `an_app_files_write_upgrade_is_plain_busy_because_it_is_not_in_wal` (`:10806`) |
| `ReattachFile` swaps the file with both connections detached first | unit tests in `src/zeroship-data-sqlite/src/session.rs` (`:2785+`) |

Three notes on what these arms do **not** establish, so nobody reads them for
more than they carry:

- The concurrency arm and the `SQLITE_BUSY_SNAPSHOT` arm both operate on `main`,
  the session's own WAL database. They prove the error mapping and the lane
  mechanics; they prove nothing about app data, where journal mode is DELETE.
  The two are paired deliberately: the app-file control is what keeps the WAL arm
  from silently describing a world we do not run in.
- Dropping a caller-side future **races** the command's completion, and the actor
  resolves that race with a terminal CAS it alone owns: cancellation wins ->
  interrupt, roll the reservation's connection back, retire, then acknowledge;
  completion wins -> `AlreadyCompleted`, and **no rollback is claimed**. An
  unconditional "a drop rolls back" cannot be implemented and must not be
  promised: the actor may commit and reply before the caller ever polls, and
  nothing dropped afterwards can un-commit it. Worse, such a promise invites a
  caller to treat a dropped future as proof the write did not land, which is the
  most dangerous thing this protocol could tell anyone. That is why the protocol
  has an explicit `Cancel` command rather than treating a drop as one - a drop is
  not observable by the actor at all.
- The classifier's unit arms do not establish that the actor calls the
  classifier at the right moment, or that quarantine recycles the connection.
  Those are actor-level and covered in `session.rs` and the integration target.
