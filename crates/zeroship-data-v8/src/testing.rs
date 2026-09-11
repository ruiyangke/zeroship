//! Adapter integration fixtures: install thread bindings and drive captured ORM work.
use crate::{
    compile, context, ctx_mut, metrics, system_shape_charter, transaction, tx_lanes, tx_scope,
};
use zeroship_data_orm::{
    backend,
    connection::{ConnectionFactory, LocalConnection},
    crud, encryption,
    error::DbError,
    exec,
};

/// Column-key source currently supplied to the adapter's worker thread.
pub fn isolate_key_source() -> encryption::ProjectKeySource {
    context::isolate_key_source()
}

#[doc(hidden)]
pub fn set_db_url_for_tests(url: &str) {
    let connection = ConnectionFactory::for_url(url).expect("valid fixture configuration");
    ctx_mut(|context| context.install_connection(connection));
}

pub fn set_backend_for_tests(backend: backend::BackendHandle, url: &str) {
    let factory = ConnectionFactory::for_url(url).expect("valid fixture configuration");
    let connection =
        LocalConnection::from_backend(factory, backend).expect("matching fixture backend");
    ctx_mut(|context| context.set_connection(connection));
}

pub fn current_backend_for_tests() -> Option<backend::BackendHandle> {
    context::with(|context| context.backend())
}

#[doc(hidden)]
pub fn reset_context_for_tests() {
    ctx_mut(|c| *c = context::ThreadDbContext::new());
    // Reset the ORM owners alongside the adapter binding for fixture isolation.
    tx_lanes::reset_for_tests();
    zeroship_data_orm::protection::mask_policy::reset_for_tests();
    metrics::reset_for_tests();
    system_shape_charter::reset_for_tests();
    zeroship_data_orm::schema_cache::reset_for_tests();
}

#[doc(hidden)]
#[must_use]
pub fn supply_project_key_for_tests(app_ids: &[&str], hex: &str) -> SuppliedProjectKeysGuard {
    let keys = std::sync::Arc::new(encryption::SuppliedProjectKeys::new());
    keys.insert_hex("fixture_project", hex).expect("fixture project key");
    for app_id in app_ids {
        keys.bind_app(app_id, "fixture_project").expect("fixture app binding");
    }
    ctx_mut(|c| c.set_supplied_project_keys(Some(std::sync::Arc::clone(&keys))));
    SuppliedProjectKeysGuard { _keys: keys }
}

#[doc(hidden)]
#[derive(Debug)]
pub struct SuppliedProjectKeysGuard {
    _keys: std::sync::Arc<encryption::SuppliedProjectKeys>,
}

impl Drop for SuppliedProjectKeysGuard {
    fn drop(&mut self) {
        ctx_mut(|c| c.set_supplied_project_keys(None));
    }
}

#[doc(hidden)]
pub fn clear_mask_policy_cache_for_tests(app_id: &str) {
    zeroship_data_orm::protection::mask_policy::cache_put(
        &zeroship_data_orm::binding::DbBinding::cold_start(app_id),
        None,
    );
}

#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn prepare_insert_many_docs_for_tests(
    docs: &mut zeroship_data_sql::value::Value,
    app_id: &str,
    collection: &str,
    actor_id: Option<&str>,
) -> Result<(), DbError> {
    let binding = zeroship_data_orm::binding::DbBinding::cold_start(app_id);
    let backend = tx_scope::ensure_backend().await?;
    let dialect = tx_scope::configured_dialect();
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

#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn finalize_rows_on_read_for_tests(
    app_id: &str,
    collection: &str,
    rows: Vec<zeroship_data_sql::value::Value>,
) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
    let binding = zeroship_data_orm::binding::DbBinding::cold_start(app_id);
    let backend = tx_scope::ensure_backend().await?;
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

#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn exec_mutation_with_emit_for_tests(
    bq: compile::BuiltQuery,
    app_id: &str,
    collection: &str,
    op: zeroship_data_orm::cdc::ChangeOp,
) -> Result<Vec<zeroship_data_sql::value::Value>, String> {
    let backend = tx_scope::ensure_backend()
        .await
        .map_err(DbError::into_string)?;
    let route = exec::ambient_route_for_tests(app_id, backend);
    exec::exec_mutation_with_emit(bq, &route, collection, op)
        .await
        .map_err(DbError::into_string)
}

#[cfg(feature = "test-helpers")]
#[doc(hidden)]
pub async fn exec_query_for_tests(
    app_id: &str,
    bq: compile::BuiltQuery,
) -> Result<Vec<zeroship_data_sql::value::Value>, String> {
    let backend = tx_scope::ensure_backend()
        .await
        .map_err(DbError::into_string)?;
    let route = exec::ambient_route_for_tests(app_id, backend);
    exec::exec_query(&route, bq)
        .await
        .map_err(DbError::into_string)
}

#[cfg(feature = "test-helpers")]
#[doc(hidden)]
#[must_use]
pub fn pool_counts_for_tests() -> Option<(usize, usize, usize)> {
    transaction::probe::pool_counts(&context::with(|c| c.backend())?)
}

#[doc(hidden)]
pub async fn begin_transaction_for_tests(app_id: &str) {
    let backend = tx_scope::ensure_backend()
        .await
        .expect("registered fixture backend");
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

/// Share explicitly supplied fixture keys with the real service composition.
#[doc(hidden)]
pub fn project_keys() -> std::sync::Arc<encryption::SuppliedProjectKeys> {
    crate::context::with(|context| context.project_keys().unwrap_or_default())
}
