#![recursion_limit = "256"]
//
// The `env.db` async chains nest deeply enough to pass rustc's default depth of
// 128 when a pooled request awaits the connection task's own future. The adapter
// carries the same attribute for the same code.

//! The PostgreSQL vendor tier of the data plane.
//!
//! # What this crate is for
//!
//! It is the only crate below `zeroship-plugin-db` allowed to name
//! `compio_postgres`. Everything above it speaks the contracts in
//! [`zeroship_data_core::storage`] - `SqlExecutor`, `LockManager`,
//! `SchemaIntrospect` and the rest - so a second backend is a sibling crate
//! rather than a set of `match` arms.
//!
//! `tests/vendor_embedding_gate.sh` enforces that mechanically: a
//! `compio_postgres::` type appearing in a signature outside this crate and its
//! SQLite peer is a build failure, not a review comment.
//!
//! # What lives here
//!
//! [`postgres::PostgresBackend`] and the five files it leans on - catalog
//! introspection, row-to-JSON decoding, SQLSTATE classification, per-app session
//! SQL, and the roled autocommit funnel - plus `lock_guard::LockGuard` (gated
//! behind `test-helpers`, so it is absent from a default-feature build), which
//! travels with the backend because its `acquire` is bound to
//! `LockManager<Client = compio_postgres::PoolConnection>`.
//!
//! # What deliberately does NOT live here
//!
//! `Backend`, the composition marker, stays in `zeroship-plugin-db`: it is that
//! crate's own `pub(crate)` trait, so by the orphan rule
//! `impl Backend for PostgresBackend` can only be written there. Its
//! compile-time conformance assertion stays with it.
//!
//! The bounded-retry lock policy is likewise absent by design - it is
//! [`zeroship_data_core::lock_policy::BoundedLockAcquire`], because the SQLite
//! backend needs it too and a policy both vendors call cannot live in either.

// These four serve only the two extension traits below, which are themselves
// gated: without the gate a default build warns on all four.
#[cfg(any(test, feature = "test-helpers"))]
use std::rc::Rc;

#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_core::error::DbError;
#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_core::storage::{LockManager, SqlExecutor};

#[cfg(any(test, feature = "test-helpers"))]
pub mod lock_guard;
pub mod pg_autocommit;
pub mod pg_error;
// The whole introspection chain follows `SchemaIntrospect`'s gate in data-core,
// and that trait is UNGATED as of 2026-09-04 - the protection floor on the write
// path reads this catalog, so it ships. Nothing new is linked by that: this
// module names `compio_postgres::Pool` and `zeroship_data_query_builder::catalog`, both already
// normal dependencies of the crate.
pub mod pg_introspect;
pub mod pg_row_json;
pub mod pg_session_sql;
pub mod postgres;

pub use postgres::PostgresBackend;

/// Postgres-specific extension trait carrying the
/// `acquire_pooled_client_for_lock` primitive - a pool checkout whose lease
/// [`crate::lock_guard::LockGuard`] holds for the life of the
/// advisory lock.
///
/// The connection owns its pool handle and returns on drop, so lock guards
/// can retain it across callbacks without a borrow of the acquiring handle.
///
/// The `: LockManager<Client = compio_postgres::PoolConnection>` super-bound
/// is load-bearing: the returned lease is the [`SqlExecutor::Client`](zeroship_data_core::storage::SqlExecutor::Client) that
/// [`LockManager::acquire_advisory_lock`](zeroship_data_core::storage::LockManager::acquire_advisory_lock) takes, so the orchestrator can hand
/// it straight into `LockGuard::acquire` without an adapter.
#[cfg(any(test, feature = "test-helpers"))]
pub trait PgLockManager: LockManager<Client = compio_postgres::PoolConnection> {
    /// Acquire a pool-leased client for advisory-lock duty.
    ///
    /// Returns a [`compio_postgres::PoolConnection`] that owns its pool handle.
    /// [`crate::lock_guard::LockGuard`] can store it across callbacks and return
    /// it on release, or discard it when lock cleanup cannot be confirmed.
    ///
    /// Postgres impl wraps `self.pool().acquire().await` and maps the
    /// pool error to [`DbError::Transient`](zeroship_data_core::error::DbError::Transient) with the same operator-
    /// facing message the bootstrap call site used to emit inline.
    #[allow(async_fn_in_trait)]
    async fn acquire_pooled_client_for_lock(
        &self,
    ) -> Result<compio_postgres::PoolConnection, DbError>;
}

/// Native PostgreSQL parameter encoding.
pub mod params;
