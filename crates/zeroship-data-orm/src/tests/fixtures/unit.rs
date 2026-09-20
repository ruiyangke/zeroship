/// Build a route for tests that return before issuing SQL. It uses the fixture’s
/// current transaction scope when one is installed. Keep the temporary directory
/// until the test has finished using the route.
///
/// # Panics
///
/// Must run inside a compio runtime because backend startup spawns a CDC publisher.
pub(crate) fn unit_route(app_id: &str) -> (crate::tx_route::TxRoute, tempfile::TempDir) {
    let (backend, dir) = unit_backend();
    (crate::exec::ambient_route_for_tests(&crate::tests::fixtures::harness_binding(app_id), backend), dir)
}

/// Open a file-backed SQLite backend with unavailable project keys.
/// Keep the returned directory while using the backend. Dropping the backend
/// requests actor shutdown without waiting for its thread to exit.
///
/// # Panics
///
/// Must run inside a compio runtime.
pub(crate) fn unit_backend() -> (crate::backend::BackendHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    // These tests do not need encryption; key resolution must fail if they request it.
    let backend = crate::backend_selection::new_sqlite_backend(
        dir.path().to_path_buf(),
        crate::encryption::ProjectKeySource::unavailable(),
    )
    .expect("open SqliteBackend");
    (
        crate::backend::BackendHandle::new(std::rc::Rc::new(backend)),
        dir,
    )
}
