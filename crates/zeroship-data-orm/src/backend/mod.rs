//! Built-in ORM adapters and their low-level capability vocabulary.
pub use crate::protection::Catalog;
#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

pub mod cancel;
pub(crate) mod identity;
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
    /// Pure compiler and storage codecs compatible with this execution host.
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration;
    /// Whether the host publishes changes from the database commit stream.
    fn publishes_committed_changes(&self) -> bool;
    /// Whether one app may hold more than one open transaction at a time.
    ///
    /// `false` for a host that reserves a single transaction connection per
    /// app: forking a second lane there could only serialize invisibly or be
    /// refused at BEGIN, so `Database::independent` refuses up front instead.
    fn admits_concurrent_transactions(&self) -> bool;
}
pub use crate::sql::descriptors::{GeoPoint, VectorMetric};
pub use crate::storage::{Backup, LockManager};
#[cfg(test)]
pub use postgres::lock_guard::LockGuard;
#[cfg(test)]
pub use postgres::{lock_guard, pg_autocommit, pg_introspect, pg_session_sql, PgLockManager};
pub use postgres::{pg_error, pg_row_json, PostgresBackend};
pub use sqlite::SqliteBackend;
#[cfg(test)]
mod tests {

    use super::*;
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
