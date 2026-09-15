//! PostgreSQL sessions, native codecs, catalog access, and ORM driver integration.

#[cfg(test)]
use zeroship_data_orm::error::DbError;
#[cfg(test)]
use zeroship_data_orm::storage::LockManager;

#[cfg(test)]
pub mod lock_guard;
pub mod pg_autocommit;
pub mod pg_error;
// Catalog access also serves runtime protection checks.
pub mod implementation;
pub mod pg_introspect;
pub mod pg_row_json;
pub mod pg_session_sql;

pub use implementation::*;

/// Test-host lock support using an owned PostgreSQL pool lease. The lock manager
/// and guard share the same client type so acquisition and release use one session.
#[cfg(test)]
pub trait PgLockManager: LockManager<Client = compio_postgres::PoolConnection> {
    /// Acquire a lease for advisory locking. Return it only after unlock succeeds;
    /// discard it when cleanup cannot be confirmed.
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
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration {
        crate::sql::registration::SqlRegistration::postgres()
    }

    fn publishes_committed_changes(&self) -> bool {
        false
    }

    /// Each transaction holds a pool lease of its own, so the ceiling is the
    /// pool's size rather than one per app.
    fn admits_concurrent_transactions(&self) -> bool {
        true
    }
}
