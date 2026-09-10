//! PostgreSQL sessions, native codecs, catalog access, and ORM driver integration.

#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_orm::error::DbError;
#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_orm::storage::LockManager;

#[cfg(any(test, feature = "test-helpers"))]
pub mod lock_guard;
pub mod pg_autocommit;
pub mod pg_error;
// The whole introspection chain follows `Catalog`'s gate in data-core,
// and that trait is UNGATED as of 2026-09-04 - the protection floor on the write
// path reads this catalog, so it ships. Nothing new is linked by that: this
// module names `compio_postgres::Pool` and `zeroship_data_sql::catalog`, both already
// normal dependencies of the crate.
pub mod implementation;
pub mod pg_introspect;
pub mod pg_row_json;
pub mod pg_session_sql;

pub use implementation::*;

/// Postgres-specific extension trait carrying the
/// `acquire_pooled_client_for_lock` primitive - a pool checkout whose lease
/// [`crate::backend::postgres::lock_guard::LockGuard`] holds for the life of the
/// advisory lock.
///
/// The connection owns its pool handle and returns on drop, so lock guards
/// can retain it across callbacks without a borrow of the acquiring handle.
///
/// The `: LockManager<Client = compio_postgres::PoolConnection>` super-bound
/// is load-bearing: the returned lease is the [`DatabaseFixture::Client`](zeroship_data_orm::storage::DatabaseFixture::Client) that
/// [`LockManager::acquire_advisory_lock`](zeroship_data_orm::storage::LockManager::acquire_advisory_lock) takes, so the orchestrator can hand
/// it straight into `LockGuard::acquire` without an adapter.
#[cfg(any(test, feature = "test-helpers"))]
pub trait PgLockManager: LockManager<Client = compio_postgres::PoolConnection> {
    /// Acquire a pool-leased client for advisory-lock duty.
    ///
    /// Returns a [`compio_postgres::PoolConnection`] that owns its pool handle.
    /// [`crate::backend::postgres::lock_guard::LockGuard`] can store it across callbacks and return
    /// it on release, or discard it when lock cleanup cannot be confirmed.
    ///
    /// Postgres impl wraps `self.pool().acquire().await` and maps the
    /// pool error to [`DbError::Transient`](zeroship_data_orm::error::DbError::Transient) with the same operator-
    /// facing message the bootstrap call site used to emit inline.
    #[allow(async_fn_in_trait)]
    async fn acquire_pooled_client_for_lock(
        &self,
    ) -> Result<compio_postgres::PoolConnection, DbError>;
}

/// Native PostgreSQL parameter encoding.
pub mod params;

pub mod driver;
mod executor;
mod protection;
mod search;

pub(crate) fn default_pool_capacity() -> usize {
    compio_postgres::PoolConfig::default().get_max_size()
}

impl crate::backend::Backend for PostgresBackend {
    fn publishes_committed_changes(&self) -> bool {
        false
    }
}
