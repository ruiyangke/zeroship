//! SQLite fixtures used by this test module.

use crate::tests::fixtures::Host;

use std::path::PathBuf;

use std::rc::Rc;

use zeroship_data_orm::backend::sqlite::SqliteBackend;

use zeroship_data_orm::backend_selection::new_sqlite_backend;

#[cfg(test)]
use crate::tests::fixtures::DatabaseFixture;

/// Spin up a fresh `SqliteBackend` rooted at a per-test temp dir.
///
/// Returns the backend + the `TempDir` guard — keep the guard alive
/// for the duration of the test so the dir survives until the
/// SqliteBackend's session drops (the worker thread closes the
/// connection on drop, which writes the final WAL checkpoint).
pub(super) fn fresh_backend(host: &Host) -> (SqliteBackend, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    let backend = new_sqlite_backend(PathBuf::from(dir.path()), host.key_source())
        .expect("open SqliteBackend");
    (backend, dir)
}

pub(super) fn compile_insert(
    namespace: &crate::sql::SchemaName,
    collection: &str,
    schema: &crate::value::Value,
    document: &crate::value::Value,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::compile::QueryError> {
    crate::crud::insert::build_one(
        namespace,
        collection,
        schema,
        document.clone(),
        &crate::sql::registration::SqlRegistration::sqlite(),
    )
}

pub(super) fn compile_insert_many(
    namespace: &crate::sql::SchemaName,
    collection: &str,
    schema: &crate::value::Value,
    documents: &crate::value::Value,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::compile::QueryError> {
    crate::crud::insert::build_many(
        namespace,
        collection,
        schema,
        documents.clone(),
        &crate::sql::registration::SqlRegistration::sqlite(),
    )
}

#[allow(clippy::too_many_arguments)]
pub(super) fn compile_find(
    namespace: &crate::sql::SchemaName,
    collection: &str,
    filter: &crate::value::Value,
    limit: Option<i64>,
    offset: Option<i64>,
    order_by: Option<&crate::value::Value>,
    select: Option<&crate::value::Value>,
    schema: &crate::value::Value,
) -> Result<crate::sql::compiler::CompiledQuery, crate::sql::compile::QueryError> {
    crate::crud::read::find(
        namespace,
        collection,
        schema,
        filter.clone(),
        limit,
        offset,
        order_by,
        select,
        &[],
        false,
        &crate::sql::registration::SqlRegistration::sqlite(),
    )
}

/// Helper — install backend + schema for an unmask test. Returns the
/// backend (kept alive for the test duration via Rc) + the TempDir
/// guard the caller binds to keep the on-disk directory alive.
pub(super) async fn unmask_setup_with_schema(
    host: &Host,
    app_id: &str,
    collection: &str,
    schema: crate::value::Value,
) -> (Rc<SqliteBackend>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let backend = Rc::new(
        new_sqlite_backend(std::path::PathBuf::from(dir.path()), host.key_source())
            .expect("SqliteBackend::new"),
    );
    backend
        .attach_app_file(app_id)
        .await
        .expect("ensure_app_schema");
    // The audit table, APPLY-AHEAD. `crud/unmask.rs` used to create it itself
    // on every dispatch; it no longer emits DDL at all, so something has to
    // stand in here for the dev-tier apply host
    // (`zeroship-migrate-node`'s `applyIrSqlite`), exactly as the
    // `apply_schema_ahead_of_runtime` fixtures stand in for it for creator
    // tables.
    //
    // These are the PRODUCTION bytes, from the production generator, not a copy
    // of them: `audit_unmask_ddl` is the same function the host calls. The
    // qualifier differs because the CONNECTION differs - the host opened the
    // app file as `main`, the worker's backend reaches it through the
    // `<app_id>` ATTACH alias - and that parameter is the only thing that
    // varies between the two callers.
    //
    // WHAT THIS FIXTURE CANNOT PROVE: that the host actually calls it. It pins
    // the shape and the writer against each other, nothing more. The call in
    // `bridge.rs::apply_ir_sqlite` is covered by no test in this file.
    for stmt in zeroship_migrate_sqlite::backend::audit_unmask_ddl(app_id) {
        backend
            .execute_fixture(&stmt, &[])
            .await
            .expect("apply-ahead: unmask audit table");
    }
    // Install into the per-isolate context so dispatch_unmask's
    // backend() lookup succeeds.
    host.install_backend(
        zeroship_data_orm::backend::BackendHandle::new(backend.clone()),
        &format!("sqlite:{}", dir.path().display()),
    );
    crate::tests::fixtures::cache_schema(app_id, collection, schema);
    (backend, dir)
}
