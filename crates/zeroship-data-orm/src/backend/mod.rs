//! Built-in ORM adapters and their low-level capability vocabulary.
pub use crate::protection::Catalog;
#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

pub mod cancel;
pub mod postgres;
pub mod sqlite;
pub use crate::backend_handle::BackendHandle;
pub use crate::capability::{BusyPolicy, ScalarRead, SnapshotHandle, SnapshotOpts, UnmaskAuditRow};
#[cfg(test)]
pub use crate::capability::{LockScope, SNAPSHOT_RESTORE_LOCK_TAG};
/// Host registration combining execution and ORM services. A connection driver
/// implements only `driver::Driver`; application services are composed here.
pub trait Backend:
    crate::executor::ScopedExecutor
    + crate::protection::Catalog
    + crate::protection::Protection
    + crate::search::Search
{
    /// Whether the host publishes changes from the database commit stream.
    fn publishes_committed_changes(&self) -> bool;
}
pub use crate::storage::{Backup, LockManager};
#[cfg(test)]
pub use postgres::lock_guard::LockGuard;
#[cfg(test)]
pub use postgres::{PgLockManager, lock_guard, pg_autocommit, pg_introspect, pg_session_sql};
pub use postgres::{PostgresBackend, pg_error, pg_row_json};
pub use sqlite::SqliteBackend;
pub use zeroship_data_sql::descriptors::{GeoPoint, VectorMetric};
#[cfg(test)]
mod tests {

    use super::*;
    use crate::error::DbError;
    fn assert_postgres_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<PostgresBackend>();
    }
    fn assert_postgres_backend_impls_sql_executor() {
        fn assert_impl<T: DatabaseFixture<Client = compio_postgres::PoolConnection>>() {}
        assert_impl::<PostgresBackend>();
    }
    fn assert_postgres_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager<Client = compio_postgres::PoolConnection>>() {}
        assert_impl::<PostgresBackend>();
    }
    fn assert_postgres_backend_impls_schema_introspect() {
        fn assert_impl<T: Catalog>() {}
        assert_impl::<PostgresBackend>();
    }
    fn assert_postgres_backend_impls_pg_lock_manager() {
        fn assert_impl<T: PgLockManager>() {}
        assert_impl::<PostgresBackend>();
    }
    #[cfg(test)]
    #[allow(dead_code)]
    fn _assert_backup<T: Backup>() {}
    #[allow(dead_code)]
    fn _assert_key_store_is_dialect_neutral() {
        fn assert_store<T: Fn(&BackendHandle) -> &crate::encryption::KeyStore>(_: T) {}
        assert_store(|handle: &BackendHandle| handle.key_store());
    }
    #[cfg(test)]
    #[allow(dead_code)]
    fn _assert_postgres_backend_impls_backup() {
        fn assert_impl<T: Backup>() {}
        assert_impl::<PostgresBackend>();
    }
    #[cfg(test)]
    #[allow(dead_code)]
    fn _assert_sqlite_backend_impls_backup() {
        fn assert_impl<T: Backup>() {}
        assert_impl::<SqliteBackend>();
    }
    fn assert_associated_types_pinned() {
        fn pinned_client<T: DatabaseFixture<Client = compio_postgres::PoolConnection>>() {}
        fn pinned_live_schema<T: Catalog>() {}
        pinned_client::<PostgresBackend>();
        pinned_live_schema::<PostgresBackend>();
    }
    fn assert_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<PostgresBackend>();
    }
    fn assert_backend_handle_clone_static() {
        fn assert_bounds<T: Clone + 'static>() {}
        assert_bounds::<BackendHandle>();
    }
    #[test]
    fn backend_handle_extension_access_type_checks() {
        fn _shape_check(handle: BackendHandle) -> bool {
            let _: Option<&PostgresBackend> = handle.get::<PostgresBackend>();
            true
        }
        let _ = _shape_check as fn(BackendHandle) -> bool;
    }
    fn assert_lock_scope_clone_send_static() {
        fn assert_bounds<T: Clone + 'static>() {}
        assert_bounds::<LockScope>();
    }
    #[test]
    fn lock_scope_keys_global_app_canonical_shape() {
        let scope = LockScope::GlobalApp {
            app_id: "app_42".to_string(),
            name: "snapshot_restore".to_string(),
        };
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_42:snapshot_restore");
        assert_eq!(k2, "snapshot_restore");
        assert_eq!(scope.app_id(), "app_42");
        assert_eq!(scope.name(), "snapshot_restore");
    }

    #[test]
    fn lock_scope_keys_local_app_canonical_shape() {
        let scope = LockScope::LocalApp {
            app_id: "app_99".to_string(),
            name: "mig:add_archived_flag".to_string(),
        };
        let (k1, k2) = scope.to_keys();
        assert_eq!(k1, "app_99:mig:add_archived_flag");
        assert_eq!(k2, "mig:add_archived_flag");
        assert_eq!(scope.app_id(), "app_99");
        assert_eq!(scope.name(), "mig:add_archived_flag");
    }
    #[allow(dead_code)]
    async fn assert_lock_scope_dispatches_through_try_acquire(
        backend: &PostgresBackend,
        client: &compio_postgres::PoolConnection,
    ) -> Result<bool, DbError> {
        let global = LockScope::GlobalApp {
            app_id: "app_t".into(),
            name: "snapshot_restore".into(),
        };
        let _ = backend.try_acquire(client, &global).await?;
        {
            use crate::lock_policy::BoundedLockAcquire;
            backend.acquire(client, &global).await?;
        }
        backend.release(client, &global).await?;
        let local = LockScope::LocalApp {
            app_id: "app_t".into(),
            name: "mig:add_archived_flag".into(),
        };
        backend.try_acquire(client, &local).await
    }

    #[test]
    fn compile_time_assertions_link() {
        let _ = assert_postgres_backend_impls_backend as fn();
        let _ = assert_postgres_backend_impls_sql_executor as fn();
        let _ = assert_postgres_backend_impls_lock_manager as fn();
        let _ = assert_postgres_backend_impls_schema_introspect as fn();
        let _ = assert_postgres_backend_impls_pg_lock_manager as fn();
        let _ = assert_associated_types_pinned as fn();
        let _ = assert_backend_is_static as fn();
        let _ = assert_backend_handle_clone_static as fn();
        let _ = assert_lock_scope_clone_send_static as fn();
    }
}
