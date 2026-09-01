//! DB-1 execution budgets: how long any one statement, lock wait, or open
//! transaction may last.
//!
//! **These are cross-backend POLICY, not PostgreSQL dialect, and that is why
//! they live here rather than beside the SQL that spends them.**
//!
//! They sat in `auth/bootstrap.rs` until 2026-09-01, interleaved with the
//! `SET LOCAL ...` builders that render them into PostgreSQL GUCs. That reads
//! as a PostgreSQL detail and is not one: `transaction/driver.rs`'s `budgets()`
//! derives the protocol execution deadline from [`DB_IDLE_IN_TX_TIMEOUT_MS`]
//! **for both backends**, deliberately, so that "the protocol deadline and the
//! server-side guard bound the same window rather than two different ones". A
//! SQLite transaction's deadline is therefore set by this constant even though
//! SQLite has no `idle_in_transaction_session_timeout` to set.
//!
//! So the split is: the NUMBERS are core policy and belong at rank 0; the
//! `SET LOCAL statement_timeout = ...` strings that spend them are PostgreSQL
//! dialect and belong with the vendor. Keeping them together would force one of
//! two wrong outcomes when the crates separate - either PostgreSQL dialect is
//! dragged into the core, or the SQLite deadline loses the constant it reads.
//!
//! Nothing here may name a driver, a dialect, or V8.

/// Max wall-time a single statement may run before the server cancels it.
pub const DB_STATEMENT_TIMEOUT_MS: u32 = 30_000;

/// Max time a connection may sit `idle in transaction` before the server
/// terminates it - the direct guard against tx-hold connection exhaustion.
///
/// Also the source of the cross-backend protocol execution deadline; see the
/// module docs before changing it, because it binds SQLite too.
pub const DB_IDLE_IN_TX_TIMEOUT_MS: u32 = 15_000;

/// Max time a statement waits on a lock before erroring (avoids lock pileups).
pub const DB_LOCK_TIMEOUT_MS: u32 = 10_000;
