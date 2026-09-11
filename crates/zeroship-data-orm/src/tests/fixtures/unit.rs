/// A real backend for units that must PASS one but never reach a statement.
///
/// The engine's row-facing entry points - `crud::read_pipeline::apply` and the
/// four `protection::unmask` dispatchers - take `&BackendHandle` as a parameter
/// rather than resolving one from the thread's context, because resolving it
/// reads ADAPTER state and those files are ENGINE. Their unit tests refuse or
/// return in a prologue that runs before the first statement, so any real
/// handle does; expressing "not opened yet" instead would need an `Option`,
/// which trades a compile error for a runtime one at every production call
/// site to spare a handful of tests a temp dir.
///
/// The `TempDir` is RETURNED rather than dropped inside this function, so the
/// directory survives the call. **What it does NOT survive is the caller's
/// scope exit, and this doc asserted the opposite until 2026-09-03.**
///
/// The tuple is `(BackendHandle, TempDir)`, so every call site spells
/// `let (backend, _dir) = unit_backend();` - and a `let` with a tuple pattern
/// declares its bindings left to right and drops locals in REVERSE declaration
/// order, so `_dir` drops FIRST and `backend` second. Scope exit therefore
/// deletes the directory while the backend is still open, which is the exact
/// inverse of "it has to outlive the backend". Measured with a compiled probe
/// rather than inferred: `let (a, b) = ...` prints `drop b` then `drop a`.
///
/// Two things keep that from being a live failure today, and NEITHER is this
/// helper's ordering:
///
/// - Every caller refuses or returns in a prologue that runs before the first
///   statement, so nothing ever reads the database file.
/// - `SqliteSession::drop` (`zeroship-data-sqlite/src/session.rs:1012`) only
///   best-effort enqueues `Shutdown` and DETACHES the worker thread without
///   joining it. The worker can therefore outlive both bindings no matter what
///   order they drop in, so no arrangement of this tuple can deliver the
///   guarantee the old wording claimed.
///
/// The change that removes the hazard is to return `(TempDir, BackendHandle)`,
/// which makes the compiler impose `let (_dir, backend) = ...` and drop the
/// backend first. It re-binds every call site, so it is not done here: the two
/// sites in `crud/read_pipeline.rs` drop explicitly instead, and the sites in
/// `crud/unmask.rs` still rely on scope exit. `crud/mask_drift.rs` was the
/// other scope-exit caller and was deleted on 2026-09-03.
///
/// # Panics
///
/// **Call this INSIDE a compio runtime** - from within `block_on`, not beside
/// it. Opening the SQLite backend spawns its CDC publisher with
/// `compio::runtime::spawn`, which panics `not in a compio runtime` off-thread
/// of one. Building the runtime first and calling this outside its `block_on`
/// is the shape that fails, and it fails on the OPEN, before any assertion.
/// [`unit_backend`], wrapped in the pool-lane route the row-facing entry points
/// now take.
///
/// `crud::read_pipeline::apply` and the three `protection::unmask` dispatchers took a
/// `&BackendHandle` until 2026-09-03 and take a `&TxRoute` now, because a
/// handle names a BACKEND and their raw-column SELECT also has to name a
/// CONNECTION. `ambient_route_for_tests` reads the parked-tx slot, which is
/// empty in every unit here, so the route binds `in_tx = false` and the reads
/// take the autocommit lane exactly as they did before.
///
/// The `TempDir` comes back for the reason [`unit_backend`] gives, and with the
/// same drop-order caveat: the route owns the handle, so a call site that needs
/// the directory to outlive the backend must drop the ROUTE explicitly first.
///
/// # Panics
///
/// As [`unit_backend`] - call it inside a compio runtime.
pub(crate) fn unit_route(app_id: &str) -> (crate::tx_route::TxRoute, tempfile::TempDir) {
    let (backend, dir) = unit_backend();
    (crate::exec::ambient_route_for_tests(app_id, backend), dir)
}

pub(crate) fn unit_backend() -> (crate::backend::BackendHandle, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("create tempdir");
    // `ProjectKeySource::unavailable()` and not the adapter's per-isolate lookup: the
    // engine cannot read `zeroship_data_v8::context` from here, and env-var
    // sourcing is exactly what that lookup returns when no fixture has supplied
    // roots - which is the state every caller of this helper is in.
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
