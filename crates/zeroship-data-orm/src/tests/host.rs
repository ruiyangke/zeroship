//! Test-owned runtime, connection and ORM state. No adapter is involved.
use crate::connection::{ConnectionFactory, LocalConnection};
use crate::{backend, compile, crud, encryption, error::DbError, exec, transaction};
use std::{cell::RefCell, rc::Rc};

struct Host {
    runtime: compio::runtime::Runtime,
    orm: crate::OrmContext,
    connection: RefCell<Option<LocalConnection>>,
    keys: RefCell<Option<Rc<encryption::SuppliedProjectKeys>>>,
}
scoped_tls::scoped_thread_local!(static ACTIVE: Rc<Host>);

/// A fixture owner spans all setup, work and teardown in a test.
pub fn in_test<T>(action: impl FnOnce() -> T) -> T {
    super::support::init_test_tracing();
    let host = Rc::new(Host {
        runtime: compio::runtime::Runtime::new().expect("fixture runtime"),
        orm: crate::OrmContext::new(),
        connection: RefCell::new(None),
        keys: RefCell::new(None),
    });
    ACTIVE.set(&host, || host.orm.with(action))
}

pub fn run<F: std::future::Future>(future: F) -> F::Output {
    ACTIVE.with(|host| host.runtime.block_on(host.orm.scope(future)))
}

impl Drop for Host {
    fn drop(&mut self) {
        self.connection.get_mut().take();
        self.keys.get_mut().take();
        self.orm.with(crate::reset_engine_for_tests);
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

pub fn isolate_key_source() -> encryption::ProjectKeySource {
    ACTIVE.with(|h| {
        h.keys
            .borrow()
            .as_ref()
            .map_or(encryption::ProjectKeySource::unavailable(), |keys| {
                encryption::ProjectKeySource::supplied(Rc::clone(keys))
            })
    })
}
pub fn set_db_url_for_tests(url: &str) {
    let factory = ConnectionFactory::for_url(url).expect("fixture connection configuration");
    ACTIVE.with(|h| {
        let mut slot = h.connection.borrow_mut();
        if slot
            .as_ref()
            .is_none_or(|c| c.factory().identity() != factory.identity())
        {
            *slot = Some(LocalConnection::new(factory));
        }
    });
}
pub fn set_backend_for_tests(backend: backend::BackendHandle, url: &str) {
    let factory = ConnectionFactory::for_url(url).expect("fixture configuration");
    ACTIVE.with(|h| {
        *h.connection.borrow_mut() =
            Some(LocalConnection::from_backend(factory, backend).expect("fixture backend"))
    });
}
pub fn current_backend_for_tests() -> Option<backend::BackendHandle> {
    ACTIVE.with(|h| {
        h.connection
            .borrow()
            .as_ref()
            .and_then(LocalConnection::backend)
    })
}
pub async fn ensure_backend() -> Result<backend::BackendHandle, DbError> {
    let connection = ACTIVE.with(|h| {
        h.connection
            .borrow()
            .clone()
            .expect("configure the fixture connection")
    });
    connection.ensure(isolate_key_source()).await
}
pub fn configured_dialect() -> compile::SqlDialect {
    ACTIVE.with(|h| {
        h.connection
            .borrow()
            .as_ref()
            .expect("fixture connection")
            .factory()
            .dialect()
    })
}
pub fn reset_context_for_tests() {
    ACTIVE.with(|h| {
        h.connection.borrow_mut().take();
    });
    crate::reset_engine_for_tests();
}
pub fn supply_project_key_for_tests(app_ids: &[&str], hex: &str) -> SuppliedProjectKeysGuard {
    let keys = Rc::new(encryption::SuppliedProjectKeys::new());
    keys.insert_hex("fixture_project", hex)
        .expect("fixture project key");
    for app_id in app_ids {
        keys.bind_app(app_id, "fixture_project")
            .expect("fixture app binding");
    }
    let host = ACTIVE.with(Rc::clone);
    let previous = host.keys.replace(Some(keys));
    SuppliedProjectKeysGuard { host, previous }
}
pub struct SuppliedProjectKeysGuard {
    host: Rc<Host>,
    previous: Option<Rc<encryption::SuppliedProjectKeys>>,
}
impl Drop for SuppliedProjectKeysGuard {
    fn drop(&mut self) {
        self.host.keys.replace(self.previous.take());
    }
}

#[doc(hidden)]
pub fn clear_mask_policy_cache_for_tests(app_id: &str) {
    zeroship_data_orm::protection::mask_policy::cache_put(
        &zeroship_data_orm::binding::DbBinding::cold_start(app_id),
        None,
    );
}

#[doc(hidden)]
pub async fn prepare_insert_many_docs_for_tests(
    docs: &mut zeroship_data_sql::value::Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    let binding = zeroship_data_orm::binding::DbBinding::cold_start(app_id);
    let backend = ensure_backend().await?;
    let dialect = configured_dialect();
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

#[doc(hidden)]
pub async fn finalize_rows_on_read_for_tests(
    app_id: &str,
    collection: &str,
    rows: Vec<zeroship_data_sql::value::Value>,
) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
    let binding = zeroship_data_orm::binding::DbBinding::cold_start(app_id);
    let backend = ensure_backend().await?;
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

#[doc(hidden)]
pub async fn exec_mutation_with_emit_for_tests(
    bq: compile::BuiltQuery,
    app_id: &str,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
) -> Result<Vec<zeroship_data_sql::value::Value>, String> {
    let backend = ensure_backend().await.map_err(DbError::into_string)?;
    let route = exec::ambient_route_for_tests(app_id, backend);
    exec::exec_mutation_with_emit(bq, &route, collection, op)
        .await
        .map_err(DbError::into_string)
}

#[doc(hidden)]
pub async fn exec_query_for_tests(
    app_id: &str,
    bq: compile::BuiltQuery,
) -> Result<Vec<zeroship_data_sql::value::Value>, String> {
    let backend = ensure_backend().await.map_err(DbError::into_string)?;
    let route = exec::ambient_route_for_tests(app_id, backend);
    exec::exec_query(&route, bq)
        .await
        .map_err(DbError::into_string)
}

#[doc(hidden)]
#[must_use]
pub fn pool_counts_for_tests() -> Option<(usize, usize, usize)> {
    transaction::probe::pool_counts(&current_backend_for_tests()?)
}

#[doc(hidden)]
pub async fn begin_transaction_for_tests(app_id: &str) {
    let backend = ensure_backend().await.expect("registered fixture backend");
    let admission = transaction::TxAdmission::acquire(app_id.to_owned()).await;
    transaction::exec_begin_or_savepoint(
        false,
        None,
        app_id,
        zeroship_data_sql::SchemaName::new(app_id).expect("fixture schema"),
        backend,
    )
    .await
    .expect("fixture BEGIN");
    admission.handed_to_reducer();
}

#[doc(hidden)]
pub async fn rollback_transaction_for_tests(app_id: &str) {
    assert!(matches!(
        transaction::exec_settle(app_id, false, None).await,
        transaction::SettleOutcome::Ok
    ));
}

#[doc(hidden)]
pub fn push_pending_emit_for_tests(ev: zeroship_data_orm::cdc::ChangeEvent) {
    crate::tx_lanes::with_mut(|l| l.push_pending_emit(ev));
}

#[doc(hidden)]
pub fn drain_pending_emits_for_tests(app_id: &str) {
    exec::drain_pending_emits_on_commit(app_id);
}

#[doc(hidden)]
pub fn clear_pending_emits_for_tests(app_id: &str) {
    exec::clear_pending_emits(app_id);
}
