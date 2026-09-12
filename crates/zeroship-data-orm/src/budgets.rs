//! Cross-backend execution and operation budgets.
//!
//! Backend-specific code enforces these values using its own transaction and
//! timeout mechanisms. This module contains no SQL or adapter policy.

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

/// Maximum documents accepted by one insert operation.
pub const MAX_INSERT_MANY_BATCH: usize = 1_000;

/// Maximum rows a write may transform individually inside one operation.
pub const MAX_PER_ROW_UPDATE_TARGETS: usize = 500;
