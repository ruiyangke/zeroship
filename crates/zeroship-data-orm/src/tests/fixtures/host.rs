//! Runtime and database state owned explicitly by each engine test.
use crate::connection::{ConnectionFactory, LocalConnection};
use crate::{backend, sql::compile, crud, encryption, error::DbError, exec, transaction};
use std::{cell::RefCell, rc::Rc, sync::Arc};

type ProjectKeys = Option<Arc<encryption::SuppliedProjectKeys>>;

pub(crate) struct Host {
    runtime: compio::runtime::Runtime,
    orm: crate::OrmContext,
    connection: RefCell<Option<LocalConnection>>,
    keys: Rc<RefCell<ProjectKeys>>,
}

impl Host {
    pub(crate) fn test<T>(action: impl FnOnce(&Self) -> T) -> T {
        super::init_test_tracing();
        let host = Self {
            runtime: compio::runtime::Runtime::new().expect("fixture runtime"),
            orm: crate::OrmContext::new(),
            connection: RefCell::new(None),
            keys: Rc::new(RefCell::new(None)),
        };
        host.orm.with(|| action(&host))
    }

    pub(crate) fn run<F: std::future::Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(self.orm.scope(future))
    }

    pub(crate) fn key_source(&self) -> encryption::ProjectKeySource {
        self.keys
            .borrow()
            .as_ref()
            .map_or_else(encryption::ProjectKeySource::unavailable, |keys| {
                encryption::ProjectKeySource::supplied(Arc::clone(keys))
            })
    }

    pub(crate) fn set_database_url(&self, url: &str) {
        let factory = ConnectionFactory::for_url(url).expect("fixture connection configuration");
        let mut slot = self.connection.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|connection| connection.factory().identity() != factory.identity())
        {
            *slot = Some(LocalConnection::new(factory));
        }
    }

    pub(crate) fn install_backend(&self, backend: backend::BackendHandle, url: &str) {
        let factory = ConnectionFactory::for_url(url).expect("fixture configuration");
        *self.connection.borrow_mut() =
            Some(LocalConnection::from_backend(factory, backend).expect("fixture backend"));
    }

    pub(crate) fn current_backend(&self) -> Option<backend::BackendHandle> {
        self.connection
            .borrow()
            .as_ref()
            .and_then(LocalConnection::backend)
    }

    pub(crate) async fn backend(&self) -> Result<backend::BackendHandle, DbError> {
        let connection = self
            .connection
            .borrow()
            .clone()
            .expect("configure the fixture connection");
        connection.ensure(self.key_source()).await
    }

    pub(crate) fn dialect(&self) -> compile::SqlDialect {
        self.connection
            .borrow()
            .as_ref()
            .expect("fixture connection")
            .factory()
            .dialect()
    }

    pub(crate) fn reset(&self) {
        self.connection.borrow_mut().take();
        self.orm.with(crate::tests::fixtures::reset_engine);
    }

    pub(crate) fn supply_project_key(
        &self,
        app_ids: &[&str],
        hex: &str,
    ) -> SuppliedProjectKeysGuard {
        let keys = Arc::new(encryption::SuppliedProjectKeys::new());
        keys.insert_hex("fixture_project", hex)
            .expect("fixture project key");
        for app_id in app_ids {
            keys.bind_app(app_id, "fixture_project")
                .expect("fixture app binding");
        }
        let previous = self.keys.replace(Some(keys));
        SuppliedProjectKeysGuard {
            slot: Rc::clone(&self.keys),
            previous,
        }
    }

    pub(crate) fn install_postgres_pool(&self, pool: Rc<compio_postgres::Pool>, url: &str) {
        let backend = backend::PostgresBackend::new(pool, url.to_owned(), self.key_source());
        self.install_backend(backend::BackendHandle::new(Rc::new(backend)), url);
    }

    pub(crate) async fn begin_transaction(&self, app_id: &str, url: &str) {
        let pool = Rc::new(
            compio_postgres::Pool::connect(url, 2)
                .await
                .expect("fixture pool"),
        );
        crate::tests::fixtures::roles::ensure_per_app_role(&pool, app_id)
            .await
            .expect("fixture role");
        if self.current_backend().is_none() {
            self.install_postgres_pool(pool, url);
        }
        self.begin_transaction_for_app(app_id).await;
    }

    pub(crate) fn clear_mask_policy_cache(&self, app_id: &str) {
        zeroship_data_orm::protection::mask_policy::cache_put(
            &zeroship_data_orm::binding::DbBinding::cold_start(app_id),
            None,
        );
    }

    pub(crate) async fn prepare_insert_many_docs(
        &self,
        docs: &mut crate::value::Value,
        app_id: &str,
        collection: &str,
        actor_id: Option<&str>,
    ) -> Result<(), DbError> {
        let binding = zeroship_data_orm::binding::DbBinding::cold_start(app_id);
        let backend = self.backend().await?;
        let dialect = self.dialect();
        // The route the V8 dispatcher would have captured. The protection-floor
        // fence reads the live catalog, so the helper has to stand in for that half
        // of the dispatcher's frame too - a helper that skipped it would let a test
        // write through a fence production applies.
        let route = exec::ambient_route_for_tests(app_id, backend.clone());
        crud::prepare_insert_many_docs_for_binding(
            backend.key_store(),
            dialect,
            &route,
            docs,
            &binding,
            collection,
            actor_id,
        )
        .await
    }

    pub(crate) async fn finalize_rows_on_read(
        &self,
        app_id: &str,
        collection: &str,
        rows: Vec<crate::value::Value>,
    ) -> Result<Vec<crate::value::Value>, DbError> {
        let binding = zeroship_data_orm::binding::DbBinding::cold_start(app_id);
        let backend = self.backend().await?;
        // The route comes from the ambient parked-tx slot rather than from a V8
        // scope, because there is no isolate here - the same trade
        // `exec_mutation_with_emit_for_tests` below documents. `apply` needs a
        // route, not a handle: its unmask stage issues SELECTs of its own and they
        // must land on the lane the read that produced these rows ran on.
        let result = crud::read_pipeline::apply(
            &exec::ambient_route_for_tests(app_id, backend),
            &binding,
            collection,
            rows,
            crud::read_pipeline::ApplyOptions::default(),
        )
        .await?;
        Ok(result.rows)
    }

    pub(crate) async fn exec_mutation_with_emit(
        &self,
        bq: compile::BuiltQuery,
        app_id: &str,
        collection: &str,
        op: zeroship_data_orm::cdc::ChangeOp,
    ) -> Result<Vec<crate::value::Value>, String> {
        let backend = self.backend().await.map_err(DbError::into_string)?;
        let route = exec::ambient_route_for_tests(app_id, backend);
        exec::exec_mutation_with_emit(
            bq,
            &route,
            collection,
            op,
            &crate::binding::DbBinding::cold_start(app_id),
        )
        .await
        .map_err(DbError::into_string)
    }

    pub(crate) async fn exec_query(
        &self,
        app_id: &str,
        bq: compile::BuiltQuery,
    ) -> Result<Vec<crate::value::Value>, String> {
        let backend = self.backend().await.map_err(DbError::into_string)?;
        let route = exec::ambient_route_for_tests(app_id, backend);
        exec::exec_query(&route, bq)
            .await
            .map_err(DbError::into_string)
    }

    pub(crate) fn pool_counts(&self) -> Option<(usize, usize, usize)> {
        transaction::probe::pool_counts(&self.current_backend()?)
    }

    pub(crate) async fn begin_transaction_for_app(&self, app_id: &str) {
        let backend = self.backend().await.expect("registered fixture backend");
        let admission = transaction::TxAdmission::acquire(app_id.to_owned()).await;
        transaction::exec_begin_or_savepoint(
            false,
            None,
            app_id,
            crate::sql::SchemaName::new(app_id).expect("fixture schema"),
            backend,
        )
        .await
        .expect("fixture BEGIN");
        admission.handed_to_reducer();
    }

    pub(crate) async fn rollback_transaction(&self, app_id: &str) {
        assert!(matches!(
            transaction::exec_settle(app_id, false, None).await,
            transaction::SettleOutcome::Ok
        ));
    }

    pub(crate) fn push_pending_emit(&self, ev: zeroship_data_orm::cdc::ChangeEvent) {
        crate::tx_lanes::with_mut(|l| l.push_pending_emit(ev));
    }

    pub(crate) fn drain_pending_emits(&self, app_id: &str) {
        exec::drain_pending_emits_on_commit(app_id);
    }

    pub(crate) fn clear_pending_emits(&self, app_id: &str) {
        exec::clear_pending_emits(app_id);
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        self.connection.get_mut().take();
        self.keys.borrow_mut().take();
        self.orm.with(crate::tests::fixtures::reset_engine);
        let drained = self.runtime.block_on(compio_postgres::drain_connections(
            std::time::Duration::from_secs(2),
        ));
        if !std::thread::panicking() {
            assert!(
                drained,
                "fixture connections must close before their runtime"
            );
        }
    }
}

pub(crate) struct SuppliedProjectKeysGuard {
    slot: Rc<RefCell<ProjectKeys>>,
    previous: ProjectKeys,
}
impl Drop for SuppliedProjectKeysGuard {
    fn drop(&mut self) {
        self.slot.replace(self.previous.take());
    }
}

#[test]
fn fixture_configuration_follows_its_owner_across_nested_scopes() {
    let directory = tempfile::tempdir().expect("fixture directory");
    Host::test(|postgres| {
        postgres.set_database_url("postgres://fixture@localhost/fixture");
        Host::test(|sqlite| {
            sqlite.set_database_url(&format!(
                "sqlite:{}",
                directory.path().join("control.sqlite").display()
            ));
            assert_eq!(postgres.dialect(), compile::SqlDialect::Postgres);
            assert_eq!(sqlite.dialect(), compile::SqlDialect::Sqlite);
            sqlite.reset();
            assert_eq!(postgres.dialect(), compile::SqlDialect::Postgres);
        });
        assert_eq!(postgres.dialect(), compile::SqlDialect::Postgres);
    });
}
