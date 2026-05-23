//! SQLite backend — skeleton.
//!
//! **P1 PR 1**: this module re-introduces the [`SqliteBackend`] type
//! and the five sub-modules (`session`, `dialect`, `lock`, `error`,
//! `fk_parse`) the plan calls out, but every capability-trait method
//! body is a stub. The intent is to lock the field set + the trait
//! impls + the `BackendHandle::Sqlite(Rc<SqliteBackend>)` arm at
//! compile time, so PR 2-5 only edit method bodies — they don't
//! re-shape the surface.
//!
//! - **PR 2**: `SqliteSession` actor + `SqlExecutor` impl + PRAGMA
//!   bootstrap + `error::from_sqlite` switch.
//! - **PR 3**: `NamespaceManager` (ATTACH) + `DialectBuilder` impl
//!   (both PG and SQLite sides, 6 hooks).
//! - **PR 4**: `LockManager` (in-process HashMap) + `SchemaIntrospect`
//!   (PRAGMA walk).
//! - **PR 5**: `IndexBuilder` + `SqliteAuditWriter` capability +
//!   cross-app FK parse-time check + `impl Backend for SqliteBackend`
//!   + the SQLite integration-test mirror.
//!
//! **`impl Backend for SqliteBackend` is intentionally absent in PR 1**
//! — the `Backend` super-trait relaxation (Q-P1-B; see
//! `docs/proposals/p1-sqlite-implementation-plan.md` §10) lands in this
//! same PR, but the marker impl waits until PR 5 ties off every
//! sub-trait. PR 1's surface is the carved capability traits only.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::Value;

use crate::backend::{
    DialectBuilder, IndexBuilder, LockManager, LockScope, NamespaceManager, SchemaIntrospect,
    SqlExecutor,
};
use crate::error::DbError;
use crate::query::IndexSpec;

pub(crate) mod dialect;
pub(crate) mod error;
pub(crate) mod fk_parse;
pub(crate) mod lock;
pub(crate) mod session;

use dialect::SqliteDialect;
use lock::InProcessLockRegistry;
use session::{SqliteSession, SqliteSessionHandle};

// The `SqliteDialect` ZST carries the canonical hook bodies; the
// backend's `impl DialectBuilder` delegates to the ZST so the
// trait-impl source-of-truth stays in one file (`dialect.rs`). No
// `dialect: SqliteDialect` field on the backend — the ZST has no
// state, so storing it would be a 0-byte field carrying no
// information. The delegation pattern below constructs the ZST
// inline (`SqliteDialect`) per call; rustc inlines the value away.

/// Sentinel returned by every capability-impl method body in PR 1.
/// PR 2-5 replace the body call with the real implementation.
fn pr_stub(method: &'static str) -> DbError {
    DbError::Internal {
        message: format!("SqliteBackend::{method} — P1 PR2+ stub"),
    }
}

/// SQLite backend handle. One instance per worker thread (mirrors
/// [`crate::backend::PostgresBackend`]'s lifecycle).
///
/// **Field set** (`docs/proposals/p1-sqlite-implementation-plan.md` §2.1):
///
/// - `session`: the writer-actor handle. Owns the single
///   `rusqlite::Connection` for this backend and serialises all DDL
///   / DML / DQL through a `flume` mpsc queue. Stubbed in PR 1.
/// - `lock_registry`: in-process advisory-lock map. Empty in PR 1;
///   PR 4 wires the `LockManager` impl through it.
/// - `db_dir`: filesystem directory holding per-app SQLite files
///   (`zs-<app_id>.sqlite`). The URL-scheme dispatcher
///   (`sqlite:///path/to/dir`) lands in P1.5.
/// - `app_id_cache`: dedup set for the `NamespaceManager::ensure_app_schema`
///   path — SQLite errors on a second ATTACH of the same alias, so
///   we filter the second call site in Rust.
///
/// **P1 PR 3 simplification**: the previous PR-1 field set carried a
/// `dialect: SqliteDialect` ZST. The ZST has no state, so the field
/// was 0 bytes carrying no information; the trait-method delegation
/// now constructs the ZST inline. See the impl block below.
#[allow(dead_code)]
pub struct SqliteBackend {
    session: Rc<SqliteSession>,
    lock_registry: Rc<InProcessLockRegistry>,
    db_dir: PathBuf,
    app_id_cache: RefCell<HashSet<String>>,
}

impl std::fmt::Debug for SqliteBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Mirrors `PostgresBackend`'s opaque Debug impl — no field
        // exposure (the `db_dir` path may carry deployment-internal
        // information operators don't want spilled to log lines).
        f.debug_struct("SqliteBackend").finish()
    }
}

impl SqliteBackend {
    /// Construct a backend rooted at `db_dir`.
    ///
    /// **P1 PR 1 stub**: opens a placeholder `SqliteSession` (which
    /// itself returns `Err(DbError::Internal { … "P1 PR2 stub" … })`).
    /// PR 2 wires the real `SqliteSession::open` + PRAGMA bootstrap.
    #[allow(dead_code)]
    pub fn new(db_dir: PathBuf) -> Result<Self, DbError> {
        // Construct a default lock registry + dialect — both are
        // PR1-safe (the lock-registry storage is empty until PR 4
        // wires consumers; the dialect is a ZST). The session is the
        // load-bearing piece — PR 2 makes this constructor actually
        // succeed.
        let session_path = db_dir.join("zs-control.sqlite");
        let session = Rc::new(SqliteSession::open(&session_path)?);
        Ok(Self {
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
        })
    }
}

// ---------------------------------------------------------------------------
// Capability impls — five carved capability blocks. PR 1 ships
// stubbed bodies; PR 2-5 backfill per `p1-sqlite-implementation-plan.md`
// §9. The order below mirrors `backend/postgres.rs` so a reviewer can
// diff the two files side-by-side as the SQLite side grows.
// ---------------------------------------------------------------------------

impl SqlExecutor for SqliteBackend {
    type Client = SqliteSessionHandle;

    async fn acquire_dedicated_client(&self) -> Result<Self::Client, DbError> {
        // SQLite has no per-client session — the actor IS the only
        // writer — so every "dedicated client" handle multiplexes
        // through the same mpsc queue. Long-lived transactions
        // serialise by construction because every command (BEGIN /
        // INSERT / COMMIT) flows through the same single-threaded
        // worker. This is the documented divergence from PG, where a
        // dedicated client gets its own libpq connection.
        Ok(SqliteSessionHandle::from(self.session.clone()))
    }

    async fn pool_exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        // "Pool" is a misnomer on the SQLite arm — there's only one
        // writer. We route directly through the session actor. The
        // PG side uses `pool.query_text_params`; the SQLite side
        // serialises via `session.exec`, which lands on the same
        // worker thread regardless of caller.
        self.session.exec(sql, params).await
    }

    async fn client_exec(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        // `client` and `self.session` point at the same actor — the
        // `Client` type is just an Rc handle. We route through the
        // handle so a future where they diverge (e.g. per-app ATTACH
        // scoping inside a transaction) can re-target without
        // touching this method.
        client.exec(sql, params).await
    }
}

impl LockManager for SqliteBackend {
    // The default-impl methods (`acquire`, `try_acquire`,
    // `release`, `try_acquire_with_backoff`) inherit through the
    // trait. PR 4 wires the three legacy string-key primitives below
    // through `InProcessLockRegistry`; until then they return the
    // PR-2+ stub sentinel.

    async fn acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        _key1: &str,
        _key2: &str,
    ) -> Result<(), DbError> {
        Err(pr_stub("acquire_advisory_lock"))
    }

    async fn try_acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        _key1: &str,
        _key2: &str,
    ) -> Result<bool, DbError> {
        Err(pr_stub("try_acquire_advisory_lock"))
    }

    async fn release_advisory_lock(
        &self,
        _client: &Self::Client,
        _key1: &str,
        _key2: &str,
    ) -> Result<(), DbError> {
        Err(pr_stub("release_advisory_lock"))
    }
}

impl NamespaceManager for SqliteBackend {
    /// Idempotently provision the per-app SQLite namespace.
    ///
    /// Constructs the per-app file path
    /// `<db_dir>/zs-<app_id>.sqlite` and routes an `ATTACH DATABASE
    /// 'file:<path>' AS "<app_id>"` through the session actor. The
    /// `app_id_cache` guards re-entry — SQLite errors on a second
    /// `ATTACH` of the same alias, so we filter the duplicate call
    /// in Rust before reaching the engine.
    ///
    /// **Lossy `PathBuf::to_string_lossy` rationale**: the per-app file
    /// path is built from `db_dir` (operator-controlled, typically a
    /// UTF-8 absolute path) joined with `zs-<app_id>.sqlite`. The
    /// `app_id` is constrained to ASCII alphanumeric + `_` + `-` by
    /// [`crate::audit::validate_app_id`] before any consumer reaches
    /// `ensure_app_schema`, so the suffix is always UTF-8 safe. If
    /// `db_dir` itself contains non-UTF-8 bytes (rare on the Linux
    /// targets we ship to), `to_string_lossy` substitutes U+FFFD —
    /// SQLite then fails to open the resulting path and surfaces a
    /// typed `DbError` on the next call. The lossy conversion is
    /// load-bearing for the actor's `String`-typed `db_path`
    /// parameter; round-tripping through OsStr would mean carrying
    /// raw bytes across an `async` boundary the actor's reply channel
    /// already serialises as `String`.
    async fn ensure_app_schema(&self, app_id: &str) -> Result<(), DbError> {
        // Idempotent guard. The cache must be checked before the
        // ATTACH because SQLite hard-errors on a duplicate ATTACH of
        // the same alias ("database <alias> is already in use"); the
        // PG side gets idempotency for free via `IF NOT EXISTS`.
        if self.app_id_cache.borrow().contains(app_id) {
            return Ok(());
        }

        // Compute the per-app file path. `to_string_lossy` is safe in
        // practice — see the rustdoc note above.
        let file_path = self.db_dir.join(format!("zs-{app_id}.sqlite"));
        let path_str = file_path.to_string_lossy().into_owned();

        // Route through the session actor's `attach` helper. The
        // actor's `run_attach` constructs the formatted ATTACH SQL
        // inline (the alias is double-quote-escaped — matches the
        // dialect's `quote_ident` byte-for-byte — and the path's
        // single quotes are doubled). The dialect's
        // `build_ensure_app_schema` is a template that pairs with
        // this helper; no PR-3 consumer routes through the template
        // path because the actor needs the file_path substituted
        // upstream anyway.
        match self.session.attach(app_id, &path_str).await {
            Ok(()) => {
                self.app_id_cache
                    .borrow_mut()
                    .insert(app_id.to_string());
                Ok(())
            }
            Err(e) => {
                // SQLite surfaces "database <alias> is already in use"
                // when an ATTACH alias collides — possible if a
                // different SqliteBackend instance attached the alias,
                // or if the cache was bypassed (test harness, future
                // pre-warm). Treat as idempotent: insert the alias
                // into the cache so subsequent calls short-circuit,
                // then return Ok. Other errors propagate verbatim.
                let msg = format!("{e}");
                if msg.contains("already in use") || msg.contains("already attached") {
                    self.app_id_cache
                        .borrow_mut()
                        .insert(app_id.to_string());
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

impl SchemaIntrospect for SqliteBackend {
    // Same associated type as the PG impl — the diff engine consumes
    // a uniform `LiveSchema` shape; the SQLite impl populates the
    // PG-style `pg_type` strings with SQLite affinity names
    // (`TEXT`/`INTEGER`/`REAL`/`BLOB`/`NUMERIC`). Classifier
    // teaching about the new vocabulary follows in a later PR.
    type LiveSchema = crate::diff::LiveSchema;

    async fn introspect_schema(&self, _app_id: &str) -> Result<Self::LiveSchema, DbError> {
        // PR 4: PRAGMA walk over `sqlite_master`, `PRAGMA table_info`,
        // `PRAGMA index_list` / `index_info`, `PRAGMA foreign_key_list`,
        // collapsed into a single `spawn_blocking` so the round-trip
        // count stays one.
        Err(pr_stub("introspect_schema"))
    }

    async fn estimate_row_count(
        &self,
        _app_id: &str,
        _collection: &str,
    ) -> Result<i64, DbError> {
        // PR 4: SQLite has no `reltuples` analogue — `SELECT COUNT(*)`
        // is fine because the only consumer needs the 0 / non-0
        // distinction (NOT NULL classifier).
        Err(pr_stub("estimate_row_count"))
    }
}

impl IndexBuilder for SqliteBackend {
    async fn create_index_with_recovery(
        &self,
        _app_id: &str,
        _collection: &str,
        _spec: &IndexSpec,
        _deploy_id: &str,
        _schema_version: i32,
    ) -> Result<(), DbError> {
        // PR 5: `CREATE [UNIQUE] INDEX IF NOT EXISTS` atomic; on
        // `SQLITE_CONSTRAINT_UNIQUE` (extended code 2067) classify
        // through `error::from_sqlite`, write a wire-compatible
        // `unique_violation` audit row, return the canonical
        // `DbError::SchemaRefused` envelope.
        Err(pr_stub("create_index_with_recovery"))
    }
}

impl DialectBuilder for SqliteBackend {
    // The backend forwards every dialect call to the `SqliteDialect`
    // ZST so consumers can hold an `&SqliteBackend` and reach the
    // dialect without naming the inner type. The ZST is instantiated
    // per call — rustc inlines the value away because every method on
    // `SqliteDialect` is `&self` and side-effect-free.

    fn quote_ident(&self, name: &str) -> String {
        SqliteDialect.quote_ident(name)
    }

    fn build_ensure_app_schema(&self, app_id: &str) -> String {
        SqliteDialect.build_ensure_app_schema(app_id)
    }

    fn build_create_index(&self, spec: &IndexSpec, online: bool) -> String {
        SqliteDialect.build_create_index(spec, online)
    }

    fn map_zs_type(&self, zs_type: &str, opts: &Value) -> String {
        SqliteDialect.map_zs_type(zs_type, opts)
    }

    fn now_fn(&self) -> &'static str {
        SqliteDialect.now_fn()
    }

    fn last_insert_rowid_sql(&self) -> Option<&'static str> {
        SqliteDialect.last_insert_rowid_sql()
    }
}

// `impl Backend for SqliteBackend {}` is intentionally absent at PR 1.
// PR 5 ties off every sub-trait + restores the marker impl. Routing
// the field through `BackendHandle::Sqlite(Rc<SqliteBackend>)` does
// NOT depend on the `Backend` marker (the `BackendHandle::with_sqlite`
// / `as_sqlite` accessors hand out `&SqliteBackend` directly).
//
// Why defer the marker? See §10 Q-P1-B in the implementation plan:
// the `Backend` super-trait relaxation lands in PR 1 (this commit)
// but the sub-trait impl set isn't real until PR 5. Adding the
// marker now would type-check (all sub-trait impls are present in
// stub form) but mislead any reader who searches for the impl to
// find a runtime-meaningful backend. Wait until PR 5 lights up the
// behaviour before naming it `: Backend`.

// `LockScope` is re-exported here so PR 4's `LockManager` impl can
// pull it in without crossing module boundaries. PR 1 does not
// reference it directly; the `#[allow(unused_imports)]` documents
// the intent.
#[allow(unused_imports)]
use super::LockScope as _LockScopeForPr4;

// Silence unused-import warnings on the LockScope item until PR 4
// wires the typed-lock primitives. The import above is the load-
// bearing one; this line ensures PR 1 builds clean.
#[allow(dead_code)]
fn _phantom_lock_scope(_s: &LockScope) {}

#[cfg(test)]
mod tests {
    //! Compile-time trait-shape assertions, mirroring the PR-0 set
    //! at `crate::backend::tests` (which target `PostgresBackend`).
    //! These pin the SQLite-side surface so any future drift in the
    //! capability-trait composition trips compilation here rather
    //! than at a distant orchestrator / context call site.
    //!
    //! Body intentionally empty (`fn assert_impl<T: Trait>() {}`) —
    //! the bound itself is the assertion.

    use super::*;
    use crate::backend::{
        DialectBuilder, IndexBuilder, LockManager, NamespaceManager, SchemaIntrospect,
        SqlExecutor,
    };

    fn assert_sqlite_backend_impls_sql_executor() {
        fn assert_impl<T: SqlExecutor>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_namespace_manager() {
        fn assert_impl<T: NamespaceManager>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_schema_introspect() {
        fn assert_impl<T: SchemaIntrospect<LiveSchema = crate::diff::LiveSchema>>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_index_builder() {
        fn assert_impl<T: IndexBuilder>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_dialect_builder() {
        fn assert_impl<T: DialectBuilder>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<SqliteBackend>();
    }

    /// Pin the `SqlExecutor::Client` associated type to the
    /// session-handle shape. A regression that swaps the type (e.g.
    /// accidentally re-pointing it to `rusqlite::Connection` rather
    /// than the actor-handle wrapper) trips here.
    fn assert_sqlite_client_pinned_to_session_handle() {
        fn assert_impl<T: SqlExecutor<Client = SqliteSessionHandle>>() {}
        assert_impl::<SqliteBackend>();
    }

    #[test]
    fn compile_time_trait_assertions_link() {
        // Keep the asserter functions live — same convention as the
        // PG-side `compile_time_assertions_link`.
        let _ = assert_sqlite_backend_impls_sql_executor as fn();
        let _ = assert_sqlite_backend_impls_lock_manager as fn();
        let _ = assert_sqlite_backend_impls_namespace_manager as fn();
        let _ = assert_sqlite_backend_impls_schema_introspect as fn();
        let _ = assert_sqlite_backend_impls_index_builder as fn();
        let _ = assert_sqlite_backend_impls_dialect_builder as fn();
        let _ = assert_sqlite_backend_is_static as fn();
        let _ = assert_sqlite_client_pinned_to_session_handle as fn();
    }
}
