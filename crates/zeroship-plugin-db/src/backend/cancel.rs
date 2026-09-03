//! The out-of-band canceller: how forced cleanup reaches a session that some
//! other future is holding.
//!
//! ## Why this exists at all
//!
//! [`super::driver::cleanup`] can only roll a transaction back if it can reach
//! the session, and the session is in the per-app slot only when nothing is
//! running on it. A forced cleanup usually arrives at exactly the moment that is
//! false: the execution deadline fires **because a statement is slow**, and a
//! slow statement is one whose future is holding the session out of the slot
//! behind a [`crate::tx_lanes::TxClientSlotGuard`]. Without a canceller, the only
//! honest answer there is `Indeterminate`, and `Indeterminate` withdraws - so
//! the mechanism whose whole purpose is to bound a slow statement responded by
//! destroying the connection, every time.
//!
//! A canceller does not need the session. PostgreSQL's `CancelRequest` is sent
//! on a **second, dedicated connection** and names the backend by process id
//! plus secret key; SQLite's is a message to the session actor plus
//! `sqlite3_interrupt` on the target connection. Both are capturable while we
//! still own the client, and usable long after some other future has taken it.
//!
//! ## The delivery barrier is the load-bearing part
//!
//! [`TxCanceller::cancel`] uses `Pool::cancel_query`, which sends the packet and
//! then **waits for the postmaster to close the cancellation connection**
//! (`cancel_query_raw::wait_for_server_close`). That EOF is a cross-connection
//! ordering barrier: PostgreSQL closes that socket only after consuming the
//! startup packet, so once it returns, the cancel cannot still be in flight and
//! land on a later statement. A fire-and-forget send would let a delayed
//! `CancelRequest` arrive after the cleanup `ROLLBACK`, after the lease went
//! back to the pool, and cancel *the next borrower's* query. The pool's own
//! command-timeout recovery makes the same choice for the same reason
//! (`libs/compio-postgres/src/pool.rs`, `run_pool_command`).
//!
//! ## The token cannot outlive its lease, and that is enforced elsewhere
//!
//! A [`compio_postgres::CancelToken`] taken from a pooled borrow carries the
//! lease's `PoolCancelLease`. `Pool::return_client` revokes it, so a canceller
//! held past the end of the transaction refuses with "its pool lease has ended"
//! rather than cancelling whatever the next borrower is running. This module
//! therefore does not have to police its own lifetime for *safety*; it is
//! dropped at [`crate::context::ThreadDbContext::retire_transaction`] for
//! tidiness, not for correctness.

use std::rc::Rc;

use compio_postgres::{CancelToken, Pool};

use crate::backend::sqlite::reservation::TerminalOutcome as SqliteTerminalOutcome;
use crate::backend::sqlite::session::SqliteCancelHandle;
use crate::tx_lanes::TxConnection;
use zeroship_data_core::error::DbError;

/// What one delivered cancellation accomplished.
///
/// The two backends differ in how much of the cleanup a cancel performs, and
/// collapsing that difference is how a driver ends up claiming a rollback it
/// never made.
#[derive(Debug)]
pub(crate) enum CancelDelivery {
    /// **PostgreSQL.** The request was delivered and the postmaster closed the
    /// cancellation connection. Nothing is proved about the transaction: the
    /// statement's own `57014` failure is what frees the session, and the
    /// caller must still reclaim it and issue the cleanup `ROLLBACK`.
    Requested,
    /// **SQLite.** The actor answers a `Cancel` only after it has rolled back
    /// and retired the reservation, so this outcome IS the cleanup result.
    SqliteSettled(SqliteTerminalOutcome),
}

/// The capability to cancel whatever one app's transaction session is running.
///
/// Cloned out of the context before every use: `cancel` is `async`, and a
/// `RefCell` borrow of [`crate::context::ThreadDbContext`] must never be held
/// across an await. Every field is itself a cheap handle - an `Rc`, an `Arc`
/// and a `Bytes` - so the clone copies no connection state.
/// The PostgreSQL half of [`TxCanceller`], boxed.
///
/// `CancelToken` carries a whole `SocketConfig` - address, hostname, keepalive,
/// TLS policy identity - and its SQLite sibling is two `Arc`s and a channel
/// sender. Inline, every value of the enum would pay the PostgreSQL size,
/// including on the dev tier where the variant is never constructed. One heap
/// allocation per transaction BEGIN buys that back, on a path that has already
/// done a pooled checkout and a network round trip.
#[derive(Clone)]
pub(crate) struct PostgresCanceller {
    /// The pool the lease came from, so the cancellation connection is opened
    /// with the same TLS connector the session was.
    pool: Rc<Pool>,
    token: CancelToken,
}

#[derive(Clone)]
pub(crate) enum TxCanceller {
    Postgres(Box<PostgresCanceller>),
    Sqlite(SqliteCancelHandle),
}

impl std::fmt::Debug for TxCanceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Postgres(_) => f.debug_struct("TxCanceller::Postgres").finish(),
            Self::Sqlite(_) => f.debug_struct("TxCanceller::Sqlite").finish(),
        }
    }
}

impl TxConnection {
    /// Capture the canceller for this session, before it is installed.
    ///
    /// Taken at install time and not at cancel time, because at cancel time the
    /// client is exactly what we do not have.
    ///
    /// `None` means this session cannot be cancelled at all: a SQLite handle
    /// that holds no transaction reservation has nothing stable to interrupt
    /// (its autocommit reservation is minted per command). A PostgreSQL session
    /// always yields a token; whether the server issued a usable secret key is
    /// decided later, by `cancel_query` itself.
    ///
    /// The fifth and last of the lane's operations, beside `exec`, `settle`,
    /// `cleanup` and `destroy`: the driver asks the session for its canceller
    /// rather than matching the session to build one.
    pub(crate) fn canceller(&self) -> Option<TxCanceller> {
        match self {
            Self::Postgres(pg) => Some(TxCanceller::Postgres(Box::new(PostgresCanceller {
                pool: Rc::clone(pg.pool()),
                token: pg.cancel_token(),
            }))),
            Self::Sqlite(handle) => handle.cancel_handle().map(TxCanceller::Sqlite),
        }
    }
}

impl TxCanceller {
    /// Deliver the cancellation, and wait for the backend to acknowledge that it
    /// has it.
    ///
    /// # Errors
    ///
    /// The backend's own refusal. Notably `CancelToken::cancel_query` refuses
    /// when the pool lease has already ended, which is the case where the
    /// session is no longer ours to cancel - so an error here is a reason to
    /// answer `Indeterminate`, never to retry against a fresh token.
    pub(crate) async fn cancel(&self) -> Result<CancelDelivery, DbError> {
        match self {
            Self::Postgres(postgres) => {
                let PostgresCanceller { pool, token } = postgres.as_ref();
                // `Pool::cancel_query` waits for the postmaster to close the
                // cancellation connection. See the module header: that EOF is
                // the ordering barrier that keeps a delayed cancel from
                // reaching a later borrower's query.
                pool.cancel_query(token).await.map_err(|error| {
                    DbError::internal(format!(
                        "db.transaction: could not deliver a cancellation request: {error}"
                    ))
                })?;
                Ok(CancelDelivery::Requested)
            }
            Self::Sqlite(handle) => handle.cancel().await.map(CancelDelivery::SqliteSettled),
        }
    }
}
