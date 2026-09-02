# The SC-1 vendor lane: what a transaction backend owes the protocol

Status: proposed, 2026-09-02. Closes #122. Blocks the `transaction/` half of the
data-crate split.

## The question

`crates/zeroship-plugin-db/src/transaction/` must divide between two crates: the
SC-1 protocol (vendor-neutral, engine tier) and the mechanics that talk to a
database (vendor tier). #122 states the division as a slogan - "transaction
mechanics go to the backend, the SC-1 protocol does not" - without saying where
the line falls. This proposal draws it.

## What is already clean, measured

`transaction/reducer/` is **4,886 lines across five files with zero
`compio_postgres`, zero `rusqlite` and zero `v8` occurrences.** The state
machine, the frame stack, the deadline slot and the lifecycle classifier are
already vendor-neutral and V8-neutral. Nothing in this proposal touches them.

The V8 half left on 2026-09-02 (`5bf9614d4`): eleven items moved to
`v8_classes/transaction.rs`, and `transaction/` now names no `v8::` type in any
signature.

So the remaining question is only about `driver.rs`, `mod.rs`, `cancel.rs` and
`probe.rs`.

## What is not clean, measured

The tier census reports two signature-position violations, but those are the tip.
The full vendor surface is:

| site | what it is |
| --- | --- |
| `mod.rs:184` `apply_per_app_role(client: &compio_postgres::Client, ..)` | `SET LOCAL ROLE` + the DB-1 timeout guards. PostgreSQL-only: SQLite has no roles. |
| `mod.rs:519` `build_begin_sql(isolation_level)` | Renders `BEGIN ISOLATION LEVEL X`. PostgreSQL dialect. |
| `driver.rs:72` `use compio_postgres::TransactionStatus` | The post-failure oracle in `terminal`. |
| `driver.rs:690` `open_session` | Matches `BackendHandle`, acquires a dedicated client, sends BEGIN, applies the role, installs. |
| `driver.rs:766` `terminal` | Matches `TxConnection`; PG reads the command tag, SQLite calls `handle.settle`. |
| `driver.rs:823` `sqlite_terminal` | Projects SC-2's `TerminalOutcome` onto SC-1's. |
| `driver.rs:1023` `rollback_session_in_slot` | Matches `TxConnection` to pick a cleanup. |
| `driver.rs:1139` `cleanup_postgres(&compio_postgres::Client)` | ROLLBACK, then sample the `Idle` oracle. |
| `driver.rs:1165` `cleanup_sqlite(&SqliteSessionHandle)` | The SQLite twin. |
| `cancel.rs` | `CancelToken` / `SqliteCancelHandle` capture. |

## The defect that names the seam

`StepConfig` carries `begin_sql: Option<String>` (`driver.rs:180`). It is
rendered by `build_begin_sql` at `driver.rs:198`, threaded through the reducer's
step configuration, and read back at `:449` before `open_session`.

**The SQLite arm ignores it.** `open_session` sends the rendered string to
Postgres (`:697`) and a hardcoded `"BEGIN"` to SQLite. So a PostgreSQL dialect
artifact is carried through the SC-1 state machine's configuration for the
benefit of exactly one of the two backends.

That is the seam, stated precisely: **the protocol currently transports SQL. It
should transport intent.**

## The shape

One vendor lane, five operations. Everything the SC-1 driver needs from a
backend, and nothing more:

```rust
/// Opening is the backend's business, because what "open" means differs:
/// PostgreSQL checks out of a pool and narrows the role; SQLite attaches the
/// app file and takes the single writer.
async fn open(&self, app_id: &str, begin: BeginIntent) -> Result<TxLane, OpenOutcome>;

/// The five things a lane does once open.
impl TxLane {
    async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError>;
    async fn settle(&self, intent: SettleIntent) -> (TerminalResult, Option<DbError>);
    async fn cleanup(&self, goal: CleanupGoal) -> CleanupAck;
    fn canceller(&self) -> Option<TxCanceller>;
    fn destroy(self);
}
```

Every type crossing that boundary is protocol vocabulary the reducer already
owns - `SettleIntent`, `TerminalResult`, `CleanupGoal`, `CleanupAck`,
`FrameId` - plus one new one:

```rust
/// What the creator asked for, not how a dialect spells it.
enum BeginIntent { Default, Isolation(IsolationLevel) }
enum IsolationLevel { ReadUncommitted, ReadCommitted, RepeatableRead, Serializable }
```

`build_begin_sql` moves into the PostgreSQL lane and renders `BeginIntent`.
The SQLite lane ignores the isolation arm, which is
`docs/reference/sqlite-divergences.md`'s existing documented divergence rather
than a new one. `StepConfig::begin_sql: Option<String>` becomes
`BeginIntent`, and the protocol stops carrying SQL.

## Enum, not `dyn Trait`

`TxConnection` (`context.rs:79`) is already this lane in enum form, and on
2026-09-02 it grew the first of the five methods (`exec`, commit `c2bd6bd99`).
Completing it means adding `settle`, `cleanup`, `canceller` and `destroy`, and
deleting the corresponding `match` in the driver.

Keep the enum. Two ungated variants, no extensibility requirement, no dyn
dispatch on a path that runs per statement - and it is the same shape
`BackendHandle` already uses, which #112 established is deliberate. The split
question is not "enum or trait", it is **which crate the enum is compiled into**,
and the answer is the vendor tier.

The rule that falls out, and that the census can check: **the driver calls
methods on a lane; it never matches a lane's variants.** Every `match` in the
table above disappears or moves.

## What does NOT move

- The reducer. All 4,886 lines stay.
- `run_operation`, `open_frame`, `close_frame`, `settle_root`, `cancel`,
  `deadline_fired`, `step`, `apply`, `run` - the action pump. It decides WHICH
  operation; the lane performs it.
- `protocol_error`, `outcome_error`, the three `*_indeterminate` mappers.
- The SEC-1 app-keying. The lane never sees another app: `install`/`take` are
  app-scoped in `context.rs` and stay there.

## The one judgement call

`terminal`'s PostgreSQL arm reads the command tag because **PostgreSQL answers
`COMMIT` with the tag `ROLLBACK` when the transaction is in a failed state**, and
a driver that reads only "did it error" reports a discarded transaction as
committed. That check is vendor knowledge and moves into the PG lane.

But the THREE-WAY classification it feeds - Committed / RolledBack /
Indeterminate - is SC-1's, and DBR-03 says an absent client is not proof the
transaction ended. So the lane returns `(TerminalResult, Option<DbError>)`: the
vendor decides what happened, the protocol decides what it means.

`sqlite_terminal` is already exactly this - a pure projection from SC-2's
`TerminalOutcome` onto SC-1's. **PostgreSQL has no such function; its projection
is inlined in `terminal`.** Giving PG the twin SQLite already has is the whole
of this step.

## Order

1. **DONE, `28a59bea2`.** Give PostgreSQL the terminal projection `sqlite_terminal`
   already had. Split in two - `pg_terminal_from_tag` and
   `pg_terminal_from_status` - because `compio_postgres::Error` has no public
   constructor, so a single error-taking function could not be unit-tested at
   all. Five pure arms now cover L8, the `RELEASE` control, and DBR-03.

1a. **DONE, `be58e73ee`, AND THIS STEP WAS MISSING FROM THE FIRST DRAFT.** Move
   `SettleIntent` and `TerminalResult` to data-core. The draft said "move
   `settle` onto `TxConnection`" as though the types were already placeable, but
   they are owned by `transaction/reducer/mod.rs`, which is ENGINE. A lane
   naming them would reach UP into the protocol - the exact inversion #103 fixed
   by moving `DenyReason` DOWN, on the rule "the engine keeps the logic, the core
   keeps the vocabulary". The reducer re-exports both, so its call sites are
   unchanged.

2. **DONE, `f682673e4`.** Was blocked on #100, which is now decided (`context.rs`
   is the composition root, so a method on `TxConnection` is an adapter concern
   and commits to nothing further). `TxConnection::settle` owns the dispatch and
   `terminal` no longer matches variants.

   **It went further than the draft said, and had to.** Moving only the method
   would have left `pg_terminal_from_tag`, `pg_terminal_from_status` and
   `sqlite_terminal` in the ENGINE - three functions whose whole content is
   vendor knowledge, two of which name `compio_postgres::TransactionStatus` and
   `rusqlite`-derived outcomes in their signatures. They moved DOWN to the
   backends that produce them (`backend::postgres::terminal_from_{tag,status}`,
   `backend::sqlite::reservation::terminal_result`), which also keeps the
   driver's two cleanup call sites legal: engine calling vendor is downward.
   The five pure L8 arms moved with them and still bind there.

   Measured: `driver.rs` vendor mentions 14 -> 12, and `crate::backend::pg_error`
   became an unused import - the driver no longer classifies a PostgreSQL error
   at all on this path.

3. Move `cleanup` onto `TxConnection`; delete `rollback_session_in_slot`'s match.
   `cleanup_postgres` and `cleanup_sqlite` become lane methods. **Unblocked.**
   This is now the largest remaining match: `driver.rs:961-962` plus the two
   functions at `:1077` and `:1103`.
4. Introduce `BeginIntent`; move `build_begin_sql` into the PG lane; change
   `StepConfig`.
5. Fold `apply_per_app_role` into the PG lane's `open`. It is already only
   called from `open_session`.
6. `cancel.rs` becomes the lane's `canceller()`.

Each step compiles and keeps the suite green on its own. Steps 1-3 remove both
census violations; 4-6 remove the coupling the census cannot see.

## Verification

Per step: `cargo check -p zeroship-plugin-db --all-targets` **and**
`--all-targets --features test-helpers` (they are different builds since
`b589cabe9`), `cargo test -p zeroship-plugin-db --lib`, and
`cargo check -p zeroship-worker -p zeroship-runtime --all-targets` for
dependents - `cargo check -p` alone is blind to both test cfg and dependents
(#145).

End state, checkable: `grep -c 'compio_postgres\|backend::sqlite' transaction/`
returns 0 outside `#[cfg(test)]`, and `tests/lib/tier_signature_census.sh`
reports no `transaction/` row.

**The census is necessary and not sufficient here.** It scans signature
positions, so it never saw `StepConfig::begin_sql` - a `String` field carrying
PostgreSQL dialect - and will not see it leave. Step 4 must be verified by
reading the type, not by watching the number.
