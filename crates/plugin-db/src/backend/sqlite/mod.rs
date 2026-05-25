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
//! **`impl Backend for SqliteBackend` lands in P1 PR 5**: every
//! sub-trait now carries a real (non-stub) impl, so the composition
//! marker `impl Backend for SqliteBackend {}` is added at the bottom
//! of this file. The relaxation of the `Backend` super-bound to drop
//! the `Client = compio_postgres::Client` pin landed in PR 1; PR 5 is
//! the moment the marker actually wires up.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::Value;
use tempfile::TempDir;

use crate::backend::{
    AuditWriter, Backend, DialectBuilder, IndexBuilder, LockManager, NamespaceManager,
    SchemaIntrospect, SqlExecutor,
};
use crate::error::DbError;
use crate::query::IndexSpec;

// `cdc` is the P2 PR-1+ home for the SQLite-side `ChangeStream`
// adapter (the `preupdate_hook` install + worker→compio publisher
// integration lands in PR 2; PR 1 ships the stub). Crate-private —
// the public consumer surface is
// `BackendHandle::as_change_stream_sqlite()` (mirroring the
// `as_postgres` / `as_sqlite` accessor shape).
pub(crate) mod cdc;
pub(crate) mod dialect;
pub(crate) mod error;
// **P4 PR 5** — FTS5 vtable lifecycle + MATCH query composition. The
// `impl FullTextIndex for SqliteBackend` block at the bottom of this
// file orchestrates the five idempotent DDL statements + the search
// path; the SQL primitives (`build_create_fts_table_sql`,
// `build_insert_trigger_sql`, `build_fts_search_sql`, etc.) live in
// `fts.rs` so the documented shapes stay unit-testable in isolation.
pub(crate) mod fts;
pub(crate) mod lock;
// **P5 PR 3.5** — `pub` under `test-helpers` so the e2e encrypted-
// column round-trip test in `tests/sqlite_integration.rs` can name
// `session::TypedCell` for typed BLOB extraction.
#[cfg(not(feature = "test-helpers"))]
pub(crate) mod session;
#[cfg(feature = "test-helpers")]
pub mod session;
// **P4 PR 5** — pure-Rust haversine + `(lat, lng)` BLOB round-trip.
// The `impl SpatialIndex for SqliteBackend` block at the bottom of
// this file routes the flat-scan path through this module; the math
// (`haversine_m`) and the `point_to_blob` / `blob_to_point` helpers
// stay unit-testable in `spatial.rs`.
pub(crate) mod spatial;
// **P4 PR 7** — `sqlite-vec` vec0 vtable lifecycle + MATCH query
// composition. Supersedes the P4 PR 4 pure-Rust flat scan (see
// `docs/proposals/p4-search-implementation-plan.md` §10 2026-05-24
// reassessment). The `impl VectorIndex for SqliteBackend` block at
// the bottom of this file orchestrates the five idempotent DDL
// statements + the JOIN+MATCH search path; the SQL primitives
// (`build_create_vec0_sql`, `build_*_trigger_sql`, `vec_to_le_bytes`)
// live in `vector.rs` so the documented shapes stay unit-testable
// in isolation.
pub(crate) mod vector;
// P3 PR 3: SQLite-side `SessionMinter` helpers — HMAC-SHA256 +
// bounded LRU nonce cache. The `impl SessionMinter for
// SqliteBackend` block lives at the bottom of THIS file (mirrors
// the AuditWriter/IndexBuilder convention); the helpers live in
// `session_minter.rs` so the cryptography stays out of the
// orchestration body.
pub(crate) mod session_minter;
// `fk_parse` was lifted out of this cfg-gated subtree in P1 PR 5 — the
// cross-app FK check applies on BOTH backends (PG and SQLite) so it
// lives at `crate::cross_app_fk` and is compiled unconditionally. The
// module's design lineage (SQLite ATTACH file isolation per design §18
// Q1) is documented in the new file's rustdoc.

use cdc::CommitPacket;
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

/// SQLite backend handle. One instance per worker thread (mirrors
/// [`crate::backend::PostgresBackend`]'s lifecycle).
///
/// **Field set** (`docs/proposals/p1-sqlite-implementation-plan.md` §2.1):
///
/// - `session`: the writer-actor handle. Owns the single
///   `rusqlite::Connection` for this backend and serialises all DDL
///   / DML / DQL through a `flume` mpsc queue.
/// - `lock_registry`: in-process advisory-lock map.
/// - `db_dir`: filesystem directory holding per-app SQLite files
///   (`zs-<app_id>.sqlite`).
/// - `app_id_cache`: dedup set for the `NamespaceManager::ensure_app_schema`
///   path — SQLite errors on a second ATTACH of the same alias, so
///   we filter the second call site in Rust.
/// - `_publisher`: P2 PR 2 — the worker→compio publisher task that
///   drains the dispatcher's `flume::Receiver<CommitPacket>` and
///   re-emits each event onto the thread-local broker. The
///   `JoinHandle` is held so dropping `SqliteBackend` cancels the task
///   (the task body is `while let Ok(packet) = rx.recv_async().await
///   { … }`; cancellation simply stops polling — no resources to
///   release). The matching sender lives on the writer thread, captured
///   by the three CDC hook closures; dropping the session drops the
///   connection drops the hooks drops the sender drops the channel.
#[allow(dead_code)]
pub struct SqliteBackend {
    // Held before `session` so drop order closes SQLite (and any
    // ATTACH-ed per-app files) before TempDir cleanup runs.
    memory_db_dir: Option<TempDir>,
    session: Rc<SqliteSession>,
    lock_registry: Rc<InProcessLockRegistry>,
    db_dir: PathBuf,
    app_id_cache: RefCell<HashSet<String>>,
    /// P2 PR 2: keeps the publisher task alive for the lifetime of the
    /// backend; dropped via `Drop` when the backend goes away. The
    /// `JoinHandle` is a `compio::runtime::Task<Result<(), …>>` whose
    /// `Drop` cancels the task per the `async-task` contract (see
    /// `async_task::Task` rustdoc).
    _publisher: compio::runtime::JoinHandle<()>,
    /// **P3 PR 3** — active HMAC secret for the `SessionMinter`
    /// trait impl. `None` if `ZEROSHIP_SESSION_SECRET` wasn't set
    /// at `new()` time; in that case `mint_session_token` /
    /// `init_session` fail with
    /// `DbError::Configuration { code: "not_configured" }` on
    /// first use (lazy failure — see plan §11 Q-P3-H).
    minter_secret: Option<Vec<u8>>,
    /// **P3 PR 3** — previous-generation HMAC secret for the
    /// rotation grace window. `None` if `ZEROSHIP_SESSION_SECRET_PREV`
    /// isn't set. When `Some`, `verify_signature` always evaluates
    /// both keys (no short-circuit) so timing leaks neither.
    minter_secret_prev: Option<Vec<u8>>,
    /// **P3 PR 3** — bounded LRU cache for nonce-replay detection.
    /// `Rc<RefCell<…>>` because the trait impl mutates it through
    /// an `&self` receiver. Single-threaded per worker, no atomics
    /// needed.
    nonce_cache: Rc<RefCell<session_minter::NonceCache>>,
    /// **P5 PR 3** — per-backend column-key cache. Resolves
    /// `(app_id, key_id) → AeadKey` via the `ZEROSHIP_COLUMN_KEY_<KEYID>`
    /// env var (the only sourcing variant on the SQLite arm — no
    /// admin-schema sidecar; mirrors the session-minter pattern from
    /// P3). Single-threaded (`RefCell` inside `KeyStore`) since every
    /// `SqliteBackend` is owned by a single compio thread.
    key_store: crate::encryption::KeyStore,
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
    /// Opens the control session at `<db_dir>/zs-control.sqlite` and
    /// spawns the **P2 PR 2 worker→compio publisher task** that
    /// drains the CDC dispatcher's `CommitPacket` channel and
    /// re-emits each event onto the thread-local broker.
    ///
    /// **CDC arming**: the control session installs the
    /// `preupdate_hook`/`commit_hook`/`rollback_hook` triplet on its
    /// `rusqlite::Connection`. Writes against ATTACH-ed per-app
    /// aliases (the `ensure_app_schema` path) fire the same hooks with
    /// the alias as `db_name`, so a single dispatcher serves all apps
    /// the backend hosts — no per-app session needed in PR 2. The
    /// per-event `app_id` is derived from `db_name` inside the
    /// publisher (see `cdc.rs::publisher_loop`).
    ///
    /// **Cross-thread wire**: the channel is `flume::unbounded()` per
    /// plan §11 — lock-free, structurally bounded by COMMIT cadence.
    /// Switching to a bounded + overflow-to-resync channel is a PR 3+
    /// concern if production traffic surfaces the need (plan §10
    /// Q-P2-A).
    /// **P5.5 PR 5** — accessor for the backend's filesystem root.
    /// The mask-policy sidecar file (`mask_policies.json`) lives at
    /// `<db_dir>/mask_policies.json`; the file's path is constructed
    /// from this accessor by `crate::crud::mask_policy::persist_sqlite`
    /// + `load_sqlite`.
    ///
    /// Exposed `pub(crate)` (NOT `pub`) so only the mask-policy module
    /// reaches into the backend's filesystem layout — production
    /// consumers route through `BackendHandle::Sqlite`.
    pub(crate) fn db_dir(&self) -> &std::path::Path {
        &self.db_dir
    }

    pub(crate) async fn exec_batch(&self, sql: &str) -> Result<(), DbError> {
        self.session.exec_batch(sql).await
    }

    pub(crate) async fn query_json(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<Vec<serde_json::Value>, DbError> {
        let typed = self.session.query_typed(sql, params).await?;
        Ok(crate::v8_bridge::typed_rows_to_json_value(&typed))
    }

    /// Production constructor used by the runtime URL-scheme
    /// dispatcher.
    ///
    /// `path` names the control database file for the backend. Per-app
    /// files still live beside it as `zs-<app_id>.sqlite` and are
    /// ATTACHed lazily by `ensure_app_schema`.
    ///
    /// If `path` points at an existing directory we place the control
    /// session at `<dir>/zs-control.sqlite`. `:memory:` opens the
    /// control session in SQLite's in-memory mode and keeps a
    /// `tempfile::TempDir` alive for the lifetime of the backend so
    /// the per-app ATTACH files stay ephemeral too.
    pub async fn open(path: impl AsRef<Path>) -> Result<Self, DbError> {
        let path = path.as_ref().to_path_buf();
        let opened = compio::runtime::spawn_blocking(move || Self::open_blocking(path))
            .await
            .map_err(|_| DbError::internal("SqliteBackend::open: spawn_blocking task panicked"))??;
        Ok(Self::finish_open(opened))
    }

    /// **Test helper** (P2 PR 4) — open a [`crate::backend::BrokerPauseGuard`]
    /// for `app_id`. While the returned guard is bound, the SQLite CDC
    /// publisher drops every packet whose `app_id` matches; on drop
    /// the suppression flag clears and one `Resync` is pushed per
    /// active subscription registered on the app.
    ///
    /// Gated to `cfg(any(test, feature = "test-helpers"))` so the
    /// production binary doesn't carry this convenience. The orchestrator
    /// reaches the same guard via
    /// `BackendHandle::as_change_stream_sqlite(...).pause_broker(app_id)`;
    /// the test helper exists because the integration test fixture
    /// owns the `SqliteBackend` directly rather than wrapping it in a
    /// `BackendHandle::Sqlite(Rc<...>)`.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn pause_broker_for_tests(
        &self,
        app_id: &str,
    ) -> crate::backend::BrokerPauseGuard {
        crate::backend::BrokerPauseGuard::new(app_id.to_string())
    }

    /// **Test helper** (P2 PR 4) — engage [`crate::backend::SchemaPendingGuard`]
    /// for `app_id`. While the guard is bound,
    /// [`crate::broker::Broker::try_subscribe`] returns
    /// `DbError::Coded { code: "schema_pending" }` for the app AND
    /// the SQLite CDC publisher drops every packet for the app. On
    /// drop the flag clears and one `Resync` is pushed per active
    /// subscription. Same gating + rationale as
    /// [`Self::pause_broker_for_tests`].
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn engage_schema_pending_for_tests(
        &self,
        app_id: &str,
    ) -> crate::backend::SchemaPendingGuard {
        crate::backend::SchemaPendingGuard::new(app_id.to_string())
    }

    #[allow(dead_code)]
    pub fn new(db_dir: PathBuf) -> Result<Self, DbError> {
        let session_path = db_dir.join("zs-control.sqlite");
        Self::open_with_session_path(db_dir, session_path)
    }

    fn open_blocking(path: PathBuf) -> Result<OpenedBackend, DbError> {
        let (db_dir, session_path, memory_db_dir) = if path == Path::new(":memory:") {
            let memory_db_dir = tempfile::tempdir().map_err(|e| {
                DbError::internal(format!(
                    "SqliteBackend::open: failed to create SQLite temp dir: {e}"
                ))
            })?;
            (memory_db_dir.path().to_path_buf(), PathBuf::from(":memory:"), Some(memory_db_dir))
        } else if path.is_dir() {
            let session_path = path.join("zs-control.sqlite");
            (path, session_path, None)
        } else {
            let session_path = path;
            let db_dir = session_path
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            (db_dir, session_path, None)
        };

        std::fs::create_dir_all(&db_dir).map_err(|e| {
            DbError::internal(format!(
                "SqliteBackend::open: failed to create SQLite directory {}: {e}",
                db_dir.display()
            ))
        })?;

        Self::open_session(db_dir, session_path, memory_db_dir)
    }

    fn open_with_session_path(db_dir: PathBuf, session_path: PathBuf) -> Result<Self, DbError> {
        let opened = Self::open_session(db_dir, session_path, None)?;
        Ok(Self::finish_open(opened))
    }

    fn open_session(
        db_dir: PathBuf,
        session_path: PathBuf,
        memory_db_dir: Option<TempDir>,
    ) -> Result<OpenedBackend, DbError> {
        // CDC packet channel — worker thread (producer, via commit
        // hook) → compio publisher task (consumer, calls
        // broker::publish on this thread).
        let (packet_tx, packet_rx) = flume::unbounded::<CommitPacket>();

        // Open the session WITH the packet sender so the worker
        // thread arms the hook triplet during PRAGMA bootstrap. The
        // `app_id` argument is currently unused inside the dispatcher
        // (per-event app_id derives from the hook's `db_name`
        // parameter — see `cdc::install` rustdoc), so we pass `None`.
        let session = SqliteSession::open(
            &session_path,
            None,
            Some(packet_tx),
        )?;

        Ok(OpenedBackend {
            session,
            db_dir,
            memory_db_dir,
            packet_rx,
        })
    }

    fn finish_open(opened: OpenedBackend) -> Self {
        let OpenedBackend {
            session,
            db_dir,
            memory_db_dir,
            packet_rx,
        } = opened;
        let session = Rc::new(session);

        // Spawn the publisher task on the current compio runtime. The
        // task captures `Rc<SqliteSession>` (for lazy column-name
        // resolution via `PRAGMA table_info`) + the receiver end of
        // the CDC channel. Dropping the returned `JoinHandle` cancels
        // the task; the channel sender on the worker thread will then
        // fail-fast on the next commit attempt (logged + dropped, no
        // commit veto).
        let _publisher = cdc::spawn_publisher(session.clone(), packet_rx);

        // **P3 PR 3** — SessionMinter env-var read. Missing
        // `ZEROSHIP_SESSION_SECRET` is NOT a `new()` failure: a
        // backend without an auth secret can still serve plain
        // DB ops. The lazy failure (`code: "not_configured"`)
        // surfaces on first `mint_session_token` / `init_session`
        // call. The `nonce_capacity` env override only applies
        // when the secret IS set; otherwise we provision the
        // default-capacity cache (cheap — empty `VecDeque`).
        let (minter_secret, minter_secret_prev, nonce_capacity) =
            match session_minter::SqliteSessionMinterConfig::from_env() {
                Ok(cfg) => (Some(cfg.secret), cfg.secret_prev, cfg.nonce_capacity),
                Err(_) => (None, None, session_minter::DEFAULT_NONCE_CAPACITY),
            };
        let nonce_cache = session_minter::NonceCache::new_shared(nonce_capacity);

        // **P5 PR 3** — wire the column-key store. SQLite has no
        // admin-schema sidecar (no SECURITY DEFINER getter equivalent),
        // so the only sourcing variant is `EnvVar` — mirrors the
        // session-minter pattern (P3) where the secret comes from
        // `ZEROSHIP_SESSION_SECRET`. Cache lives for the lifetime of
        // the backend; clears on backend drop.
        let key_store = crate::encryption::KeyStore::new(
            crate::encryption::KeySource::EnvVar,
        );

        Self {
            memory_db_dir,
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
            _publisher,
            minter_secret,
            minter_secret_prev,
            nonce_cache,
            key_store,
        }
    }

    /// **P3 PR 3 test helper** — construct a backend with the
    /// HMAC secret(s) supplied explicitly, bypassing the env-var
    /// read. Used by deterministic fixtures (
    /// `tests/sqlite_integration.rs` session_*` tests, the
    /// cross-backend equivalence test). Nonce-cache capacity
    /// defaults to [`session_minter::DEFAULT_NONCE_CAPACITY`].
    ///
    /// Gated to `#[cfg(any(test, feature = "test-helpers"))]` so
    /// the production binary doesn't carry the explicit-secret
    /// entry point — production paths must route through env vars
    /// so the secret bytes don't enter the crate's public API.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn new_with_secrets(
        db_dir: PathBuf,
        secret: Vec<u8>,
        secret_prev: Option<Vec<u8>>,
    ) -> Result<Self, DbError> {
        let (packet_tx, packet_rx) = flume::unbounded::<CommitPacket>();

        let session_path = db_dir.join("zs-control.sqlite");
        let session = Rc::new(SqliteSession::open(
            &session_path,
            None,
            Some(packet_tx),
        )?);

        let _publisher = cdc::spawn_publisher(session.clone(), packet_rx);

        let nonce_cache =
            session_minter::NonceCache::new_shared(session_minter::DEFAULT_NONCE_CAPACITY);

        // **P5 PR 3** — same `KeyStore::EnvVar` shape as `new()`. The
        // test helper diverges only on the session-minter secret; the
        // column-key store reads from env vars regardless.
        let key_store = crate::encryption::KeyStore::new(
            crate::encryption::KeySource::EnvVar,
        );

        Ok(Self {
            memory_db_dir: None,
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
            _publisher,
            minter_secret: Some(secret),
            minter_secret_prev: secret_prev,
            nonce_cache,
            key_store,
        })
    }
}

struct OpenedBackend {
    session: SqliteSession,
    db_dir: PathBuf,
    memory_db_dir: Option<TempDir>,
    packet_rx: flume::Receiver<CommitPacket>,
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
    // The default-impl methods (`acquire`, `try_acquire`, `release`,
    // `try_acquire_with_backoff`) inherit through the trait. The three
    // legacy string-key primitives below route through
    // `InProcessLockRegistry`:
    //
    // - `acquire_advisory_lock`: per plan §3.3, this method is
    //   essentially unused in production — every typed `acquire` call
    //   site routes through `try_acquire_with_backoff` (since [I43]).
    //   We implement it for completeness with a bounded
    //   try-then-sleep loop (20ms tick) that mirrors the PG arm's
    //   "indefinite wait" surface without the PG arm's server-side
    //   `pg_advisory_lock`. The loop has no cap — it polls forever
    //   until acquisition succeeds, matching the legacy contract.
    //
    // - `try_acquire_advisory_lock`: synchronous registry call —
    //   borrow-and-return inside one expression so the `RefCell`
    //   borrow never crosses an `.await`.
    //
    // - `release_advisory_lock`: same shape; the registry handles
    //   "release-on-unheld" as a `tracing::warn` no-op so we always
    //   return `Ok(())`.
    //
    // The `_client` argument is ignored on every primitive — design
    // §7.2: "SQLite ignores the argument". The single-writer actor
    // serialises every lock-state read/write through the same Rust
    // process, so the client identity carries no information at the
    // lock layer.

    async fn acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // 20ms poll cadence — same magnitude as the typed
        // `try_acquire_with_backoff`'s first non-zero retry step. The
        // loop is uncapped to match the legacy `pg_advisory_lock`
        // contract; typed call sites should be using
        // `try_acquire_with_backoff` (bounded at 5 attempts / ~1.75s)
        // instead. No production caller invokes this method.
        let k = (key1.to_string(), key2.to_string());
        loop {
            // Borrow scope confined to a single statement — the
            // `RefCell` is released before the `await` below.
            if self.lock_registry.try_acquire(k.clone()) {
                return Ok(());
            }
            compio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    }

    async fn try_acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<bool, DbError> {
        // Synchronous registry call. The `RefCell` borrow is contained
        // inside the `try_acquire` body and is released before this
        // function returns — there is no `.await` between borrow and
        // release.
        Ok(self
            .lock_registry
            .try_acquire((key1.to_string(), key2.to_string())))
    }

    async fn release_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // `release` is infallible at the registry layer — unheld /
        // unknown slots emit a `tracing::warn` and no-op. Returning
        // `Ok(())` unconditionally matches the legacy PG-arm contract:
        // a release on a session whose lock has already auto-released
        // (because the connection died) is also benign there.
        self.lock_registry
            .release((key1.to_string(), key2.to_string()));
        Ok(())
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

    /// Walk the SQLite catalog for `app_id`'s attached database and
    /// produce a [`crate::diff::LiveSchema`] in the same shape the PG
    /// impl emits — populated via four PRAGMA round-trips per table:
    ///
    /// 1. `SELECT name FROM "<app_id>".sqlite_master WHERE type='table'`
    ///    — table list, filtered to user tables (`sqlite_*` system
    ///    tables and our `__zs_*` bookkeeping tables are excluded).
    /// 2. `PRAGMA "<app_id>".table_info("<collection>")` — columns:
    ///    name, type, notnull (0/1), dflt_value, pk.
    /// 3. `PRAGMA "<app_id>".index_list("<collection>")` — indexes:
    ///    seq, name, unique (0/1), origin, partial. The PG impl
    ///    excludes the primary-key index (`indisprimary`); we mirror
    ///    that by skipping indexes whose `origin = 'pk'`.
    /// 4. For each non-PK index: `PRAGMA "<app_id>".index_info(...)` —
    ///    the index's column list in seqno order.
    /// 5. `PRAGMA "<app_id>".foreign_key_list("<collection>")` —
    ///    FKs: id, seq, table (target), from, to, on_update,
    ///    on_delete, match.
    ///
    /// Per plan §3.4 each PRAGMA flows through the session actor's
    /// `Query` command (one round-trip per call); the totals stay
    /// bounded at `1 + 4N` for N tables, which is fine at dev scale
    /// where this code path runs. The PG impl achieves the same with
    /// 3 SQL statements; folding the SQLite walk into a single SQL
    /// statement isn't possible (PRAGMA is non-composable), but the
    /// per-table count stays well below the orchestrator's budget for
    /// a registration round-trip.
    ///
    /// **System-table filter** (plan §3.4): drop any name beginning
    /// with `sqlite_` (engine-internal) or `__zs_` (our bookkeeping —
    /// migrations / audit / replication). The diff classifier consumes
    /// only user-declared tables; surfacing system tables would
    /// trigger spurious "drop table" classifications.
    async fn introspect_schema(&self, app_id: &str) -> Result<Self::LiveSchema, DbError> {
        let mut out = crate::diff::LiveSchema::default();

        // 1. Table list. The `app_id` is interpolated as a quoted
        //    identifier — the dialect's `quote_ident` doubles embedded
        //    `"`s; PRAGMA / sqlite_master both accept the dotted form
        //    `"app_id".sqlite_master`.
        let q_app = self.quote_ident(app_id);
        let tables_sql = format!(
            "SELECT name FROM {q_app}.sqlite_master WHERE type = 'table' ORDER BY name"
        );
        let table_rows = self.session.query(&tables_sql, &[]).await?;
        let mut user_tables: Vec<String> = Vec::with_capacity(table_rows.len());
        for row in &table_rows {
            let name = row
                .first()
                .and_then(|c| c.clone())
                .unwrap_or_default();
            // Filter out system + bookkeeping tables (plan §3.4).
            if name.starts_with("sqlite_") || name.starts_with("__zs_") {
                continue;
            }
            user_tables.push(name);
        }

        for collection in &user_tables {
            let q_coll = self.quote_ident(collection);

            // 2. Columns via `PRAGMA table_info`.
            //
            //    PRAGMA columns: 0=cid, 1=name, 2=type, 3=notnull,
            //    4=dflt_value, 5=pk. The cell shape is `Option<String>`
            //    uniformly (the session materialises every value as a
            //    stringified `Option<String>`), so we read positionally
            //    and parse the `notnull` "0"/"1" into a bool.
            let table_info_sql = format!("PRAGMA {q_app}.table_info({q_coll})");
            let col_rows = self.session.query(&table_info_sql, &[]).await?;

            // **P5 PR 3** — pull the original `CREATE TABLE` text from
            // `sqlite_master.sql` so we can recover per-column
            // encryption metadata from the `/* zsenc:<mode>:<keyId>:
            // <wraps> */` sentinel the DDL emitter writes for every
            // `t.encrypted(...)`-declared column (see
            // `crate::query::field_to_column`). PRAGMA `table_info`
            // surfaces the declared type but strips comments; the
            // sentinel only survives in `sqlite_master.sql`.
            //
            // Acknowledge: regex-on-DDL is fragile — a future SDK that
            // emits column DDL with multiple comments or non-trivial
            // line breaks could trip the per-column attachment. The
            // sidecar `__zs_schema_meta` table is the upgrade path
            // (Q-P5 deferred); same regex-on-DDL pattern as P4 PR 4's
            // vector-dims introspection.
            let master_sql_query = format!(
                "SELECT sql FROM {q_app}.sqlite_master \
                 WHERE type = 'table' AND name = ?"
            );
            let master_rows = self
                .session
                .query(&master_sql_query, &[collection.as_str()])
                .await?;
            let create_table_text: String = master_rows
                .first()
                .and_then(|r| r.first())
                .and_then(|c| c.clone())
                .unwrap_or_default();
            let encryption_by_col = parse_encryption_sentinels(&create_table_text);
            // **P5.5 PR 6** — mask sentinels (`/* __zsmask:kind=…,
            // classification=… */`) attached to `<col>_masked` sibling
            // column DDL. Same regex-on-DDL pattern P5 uses for
            // encryption sentinels.
            let mask_by_parent = parse_mask_sentinels(&create_table_text);

            let mut col_map = std::collections::HashMap::new();
            for row in &col_rows {
                let name = row
                    .get(1)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let pg_type = row
                    .get(2)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let not_null = row
                    .get(3)
                    .and_then(|c| c.as_deref())
                    .map(|s| s != "0")
                    .unwrap_or(false);
                let default_expr = row.get(4).and_then(|c| c.clone());
                let encryption = encryption_by_col.get(&name).cloned();
                let mask = mask_by_parent.get(&name).cloned();
                col_map.insert(
                    name,
                    crate::diff::ColumnInfo {
                        pg_type,
                        not_null,
                        default_expr,
                        // SQLite expression defaults are stored as raw
                        // text without a volatility tag (the engine has
                        // no `pg_proc.provolatile` analogue). Leaving
                        // this `None` matches what the PG side sets for
                        // literal defaults; the diff classifier reads
                        // `default_volatility` only when the default
                        // looks like a function call. Future PRs can
                        // pattern-match on common volatile defaults
                        // (`CURRENT_TIMESTAMP`, `(unixepoch())`, etc.).
                        default_volatility: None,
                        // P4 PR 1: new fields default; PR 5 populates
                        // `vector_dims` / `is_fts_source` / `is_geopoint`
                        // from `sqlite_master.sql` introspection regexes.
                        encryption,
                        mask,
                        ..Default::default()
                    },
                );
            }
            if !col_map.is_empty() {
                out.tables.insert(collection.clone(), col_map);
            }

            // 3. Indexes via `PRAGMA index_list` + `PRAGMA index_info`.
            //
            //    `index_list` columns: 0=seq, 1=name, 2=unique,
            //    3=origin, 4=partial. We exclude `origin='pk'` to match
            //    the PG impl's `NOT i.indisprimary` filter.
            let index_list_sql = format!("PRAGMA {q_app}.index_list({q_coll})");
            let idx_rows = self.session.query(&index_list_sql, &[]).await?;
            let mut idx_map = std::collections::HashMap::new();
            for row in &idx_rows {
                let idx_name = row
                    .get(1)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let is_unique = row
                    .get(2)
                    .and_then(|c| c.as_deref())
                    .map(|s| s != "0")
                    .unwrap_or(false);
                let origin = row
                    .get(3)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                if origin == "pk" {
                    // PG impl skips primary-key indexes; we mirror.
                    // The auto-generated `sqlite_autoindex_*` names
                    // also appear here, and they all carry origin='pk'
                    // or 'u' (unique constraint). We surface 'u'-origin
                    // indexes because they correspond to declared
                    // UNIQUE columns the diff engine cares about.
                    continue;
                }

                // 4. Columns for this index via `PRAGMA index_info`.
                //    Returns: 0=seqno, 1=cid, 2=name.
                let q_idx = self.quote_ident(&idx_name);
                let index_info_sql = format!("PRAGMA {q_app}.index_info({q_idx})");
                let info_rows = self.session.query(&index_info_sql, &[]).await?;
                let mut columns = Vec::with_capacity(info_rows.len());
                for info_row in &info_rows {
                    let col_name = info_row
                        .get(2)
                        .and_then(|c| c.clone())
                        .unwrap_or_default();
                    columns.push(col_name);
                }

                idx_map.insert(
                    idx_name,
                    crate::diff::IndexInfo {
                        is_unique,
                        columns,
                        // SQLite indexes are always considered valid
                        // once `CREATE INDEX` returns — there is no
                        // analogue to PG's `indisvalid` (which can be
                        // false after a failed `CREATE INDEX
                        // CONCURRENTLY`). Mark every observed index
                        // valid; PR 5's IndexBuilder retry logic does
                        // not need a tri-state.
                        is_valid: true,
                    },
                );
            }
            if !idx_map.is_empty() {
                out.indexes.insert(collection.clone(), idx_map);
            }

            // 5. Foreign keys via `PRAGMA foreign_key_list`.
            //
            //    Columns: 0=id, 1=seq, 2=table (target),
            //    3=from (local column), 4=to (target column),
            //    5=on_update, 6=on_delete, 7=match.
            //
            //    We synthesise a `constraint_name` from the FK id +
            //    local column (SQLite doesn't expose user-given FK
            //    names through PRAGMA — only the implicit auto-name).
            //    The PG impl uses `pg_constraint.conname` directly.
            let fk_sql = format!("PRAGMA {q_app}.foreign_key_list({q_coll})");
            let fk_rows = self.session.query(&fk_sql, &[]).await?;
            let mut fk_map = std::collections::HashMap::new();
            for row in &fk_rows {
                let fk_id = row
                    .first()
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let target_table = row
                    .get(2)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let from_col = row
                    .get(3)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let target_column = row
                    .get(4)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let on_update = row
                    .get(5)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let on_delete = row
                    .get(6)
                    .and_then(|c| c.clone())
                    .unwrap_or_default();
                let constraint_name = format!("fk_{fk_id}_{from_col}");
                fk_map.insert(
                    from_col.clone(),
                    crate::diff::ForeignKeyInfo {
                        constraint_name,
                        column: from_col,
                        target_table,
                        target_column,
                        // SQLite's PRAGMA already emits the upper-case
                        // SQL form ("CASCADE", "SET NULL", "NO
                        // ACTION", …); no decode step needed (contrast
                        // PG's single-char code).
                        on_delete,
                        on_update,
                        // SQLite FKs do not surface a deferrable bit
                        // through PRAGMA. The engine supports
                        // `DEFERRABLE INITIALLY DEFERRED` syntax but
                        // doesn't echo it back via foreign_key_list;
                        // default to `false` to match the PG impl's
                        // bool shape.
                        deferrable: false,
                    },
                );
            }
            if !fk_map.is_empty() {
                out.foreign_keys.insert(collection.clone(), fk_map);
            }
        }

        Ok(out)
    }

    /// Cheap row-count probe for the NOT-NULL-on-empty-table
    /// classifier branch.
    ///
    /// SQLite has no `pg_class.reltuples` analogue — every
    /// `SELECT COUNT(*)` is a full scan. The only consumer
    /// (`diff::classify_add_column`) needs the 0 / non-0
    /// distinction, so the scan cost is acceptable at dev scale
    /// (the orchestrator already holds the advisory lock — see plan
    /// §3.4). Future PRs can plug a `MAX(rowid)`-based fast path
    /// here if the dev-scale cost becomes an issue.
    async fn estimate_row_count(&self, app_id: &str, collection: &str) -> Result<i64, DbError> {
        let q_app = self.quote_ident(app_id);
        let q_coll = self.quote_ident(collection);
        let sql = format!("SELECT COUNT(*) FROM {q_app}.{q_coll}");
        let rows = match self.session.query(&sql, &[]).await {
            Ok(rows) => rows,
            Err(DbError::Transient { message }) if message.contains("no such table") => {
                return Ok(0);
            }
            Err(e) => return Err(e),
        };
        let n = rows
            .first()
            .and_then(|r| r.first())
            .and_then(|c| c.as_deref())
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(0);
        Ok(n)
    }
}

impl IndexBuilder for SqliteBackend {
    /// Atomic `CREATE [UNIQUE] INDEX IF NOT EXISTS` against the per-app
    /// attached database. Per plan §3.5, SQLite has no `CREATE INDEX
    /// CONCURRENTLY` analogue — the operation is atomic from the
    /// engine's view, so the PG arm's INVALID-recovery retry loop has
    /// no peer here. Either the statement succeeds or it surfaces a
    /// classified failure on the first attempt.
    ///
    /// **Error envelope** (plan §3.5 + §15.7): when `error::from_sqlite`
    /// classifies the failure as a unique-constraint violation
    /// (`SQLITE_CONSTRAINT_UNIQUE`, extended code 2067), this method
    /// writes a `unique_violation` audit row through [`AuditWriter`]
    /// then returns the canonical [`DbError::SchemaRefused`] envelope
    /// the SDK already parses on the PG side
    /// (`backend/postgres.rs::create_index_with_recovery_audited` line
    /// ~577) — the wire shape is identical so a creator's
    /// `e.code === "validation_refused"` branch handles both backends
    /// unchanged. Non-unique-constraint failures propagate verbatim;
    /// the typed `DbError` variant `error::from_sqlite` returned still
    /// stamps the canonical `.code` at the V8 boundary.
    ///
    /// **Conflicting-key extraction divergence** (plan §3.5): SQLite's
    /// `SQLITE_CONSTRAINT_UNIQUE` error does not carry the conflicting
    /// row's key value (contrast PG's 23505, which embeds it in the
    /// detail field). The envelope therefore reports
    /// `conflicting_keys: []` and points the SDK at the read path for
    /// the offending rows. Documented as an acceptable dev-tier
    /// divergence in the implementation plan.
    async fn create_index_with_recovery(
        &self,
        app_id: &str,
        collection: &str,
        spec: &IndexSpec,
        deploy_id: &str,
        schema_version: i32,
    ) -> Result<(), DbError> {
        let sql = {
            let built = self.build_create_index(spec, false);
            if !built.is_empty() {
                built
            } else {
                let q_app = self.quote_ident(app_id);
                let q_coll = self.quote_ident(collection);
                let q_idx = self.quote_ident(&spec.name);
                let cols_quoted: Vec<String> =
                    spec.columns.iter().map(|c| self.quote_ident(c)).collect();
                let col_list = cols_quoted.join(", ");
                let unique_kw = if spec.unique { "UNIQUE " } else { "" };
                format!(
                    "CREATE {unique_kw}INDEX IF NOT EXISTS {q_app}.{q_idx} ON {q_coll} ({col_list})"
                )
            }
        };

        match self.session.exec(&sql, &[]).await {
            Ok(_) => Ok(()),
            Err(err) => {
                // `session.exec` already routed the rusqlite error
                // through `error::from_sqlite`, which maps a unique-
                // constraint violation to `DbError::SchemaRefused {
                // code: "unique_violation", ... }`. We inspect that
                // structural shape here so the `unique_violation`
                // branch can also write the matching audit row + emit
                // a wire-compatible envelope (the PG arm builds it via
                // `create_index_with_recovery_audited`'s `refuse`
                // closure; we do the same shape inline).
                let unique_violation = matches!(
                    &err,
                    DbError::SchemaRefused { code, .. } if *code == "unique_violation"
                );
                if !unique_violation {
                    // Not a unique-constraint violation — propagate the
                    // classified error verbatim. The variant's `.code`
                    // already routes through `to_op_error` at the V8
                    // boundary.
                    return Err(err);
                }

                // Best-effort audit write. A failure to write the audit
                // row must not mask the underlying `unique_violation`
                // — the SDK's wire contract is the SchemaRefused
                // envelope below.
                let audit_row = crate::audit::AuditRow {
                    collection: collection.to_string(),
                    phase: crate::audit::Phase::Ddl,
                    change_class: if spec.unique {
                        crate::audit::ChangeClass::Compatible
                    } else {
                        crate::audit::ChangeClass::Additive
                    },
                    change_kind: "index_retry".to_string(),
                    details: serde_json::json!({
                        "reason": "data_violation",
                        "index_name": spec.name,
                        "columns": spec.columns,
                        "unique": spec.unique,
                        "sqlite_extended_code": "SQLITE_CONSTRAINT_UNIQUE",
                    }),
                    ddl_sql: Some(sql.clone()),
                    status: crate::audit::InitialStatus::Running,
                    deploy_id: deploy_id.to_string(),
                    schema_version,
                    actor: crate::audit::ActorKind::Auto,
                };
                if let Err(audit_err) =
                    AuditWriter::write_audit_row(self, app_id, &audit_row).await
                {
                    tracing::warn!(
                        app_id = %app_id,
                        index = %spec.name,
                        audit_err = %audit_err,
                        "SqliteBackend::create_index_with_recovery: audit write failed; \
                         falling through to SchemaRefused envelope",
                    );
                }

                // Wire-compatible envelope. The shape matches the PG
                // arm's `refuse(json!({...}))` body in
                // `backend/postgres.rs::create_index_with_recovery_audited`
                // so SDK callers see identical bytes regardless of
                // backend. `conflicting_keys: []` documents the SQLite
                // divergence (the engine does not carry the conflicting
                // row's key value through its error API).
                let envelope = serde_json::json!({
                    "code": "validation_refused",
                    "change_kind": "index_retry",
                    "collection": collection,
                    "constraint": if spec.unique { "unique" } else { "index" },
                    "index": spec.name,
                    "columns": spec.columns,
                    "conflicting_keys": [],
                    "message": err.to_string(),
                    "hint": "SQLite does not surface the conflicting row's key value; \
                             query the collection on the indexed columns to locate the duplicate.",
                });
                let envelope_json = serde_json::to_string(&envelope).unwrap_or_else(|_| {
                    String::from(
                        "{\"code\":\"validation_refused\",\
                         \"reason\":\"envelope serialisation failed\"}",
                    )
                });
                Err(DbError::SchemaRefused {
                    code: "validation_refused",
                    envelope_json,
                })
            }
        }
    }
}

// PR 2: full `AuditWriter` capability for the SQLite register-model
// pipeline — provisioning, `next_schema_version`, row insert, and
// terminal-status updates all route through the session actor.
impl AuditWriter for SqliteBackend {
    async fn ensure_audit_table(&self, app_id: &str) -> Result<(), DbError> {
        let q_app = self.quote_ident(app_id);
        let ddl = format!(
            "CREATE TABLE IF NOT EXISTS {q_app}.\"__zeroship_migrations\" (\
                 id                INTEGER PRIMARY KEY, \
                 collection        TEXT NOT NULL, \
                 phase             TEXT NOT NULL, \
                 change_class      TEXT NOT NULL, \
                 change_kind       TEXT NOT NULL, \
                 details           TEXT NOT NULL, \
                 ddl_sql           TEXT, \
                 created_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                 updated_at        TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP, \
                 applied_at        TEXT, \
                 applied_by_kind   TEXT NOT NULL, \
                 applied_by_id     TEXT, \
                 deploy_id         TEXT NOT NULL, \
                 parent_id         INTEGER REFERENCES \"__zeroship_migrations\"(id), \
                 schema_version    INTEGER NOT NULL, \
                 status            TEXT NOT NULL, \
                 error             TEXT, \
                 duration_ms       INTEGER, \
                 validate_cursor   INTEGER, \
                 owner_session_id  TEXT, \
                 last_heartbeat_at TEXT, \
                 dead_letter_pks   TEXT, \
                 audit_generation  INTEGER NOT NULL DEFAULT 0, \
                 CONSTRAINT __zeroship_migrations_phase_chk CHECK (phase IN ('ddl','validation','backfill','audit')), \
                 CONSTRAINT __zeroship_migrations_class_chk CHECK (change_class IN ('additive','compatible','destructive')), \
                 CONSTRAINT __zeroship_migrations_status_chk CHECK (status IN ('pending','running','applied','applied_with_dead_letter','failed','cancelled','rolled_back','validation_refused'))\
             );\
             CREATE INDEX IF NOT EXISTS {q_app}.\"__zeroship_migrations_deploy_idx\" \
                 ON \"__zeroship_migrations\" (deploy_id);\
             CREATE INDEX IF NOT EXISTS {q_app}.\"__zeroship_migrations_updated_at_idx\" \
                 ON \"__zeroship_migrations\" (updated_at DESC);"
        );
        self.session.exec_batch(&ddl).await
    }

    async fn next_schema_version(&self, app_id: &str) -> Result<i32, DbError> {
        let q_app = self.quote_ident(app_id);
        let sql = format!(
            "SELECT COALESCE(MAX(schema_version), 0) + 1 \
             FROM {q_app}.\"__zeroship_migrations\" \
             WHERE phase = 'ddl' AND status = 'applied'"
        );
        let rows = self.session.query(&sql, &[]).await?;
        let value = rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_deref())
            .ok_or_else(|| DbError::internal("sqlite audit: missing schema_version row"))?;
        value.parse::<i32>().map_err(|e| {
            DbError::internal(format!(
                "sqlite audit: invalid schema_version {value:?}: {e}"
            ))
        })
    }

    async fn write_audit_row_returning_id(
        &self,
        app_id: &str,
        row: &crate::audit::AuditRow,
    ) -> Result<i64, DbError> {
        let q_app = self.quote_ident(app_id);
        let sql = format!(
            "INSERT INTO {q_app}.\"__zeroship_migrations\" \
                (collection, phase, change_class, change_kind, details, \
                 ddl_sql, status, deploy_id, applied_by_kind, schema_version) \
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10) \
            RETURNING id"
        );

        let details_str = row.details.to_string();
        let schema_version_str = row.schema_version.to_string();
        let ddl_sql_str = row.ddl_sql.clone().unwrap_or_default();
        let params: [&str; 10] = [
            row.collection.as_str(),
            row.phase.as_sql(),
            row.change_class.as_sql(),
            row.change_kind.as_str(),
            details_str.as_str(),
            ddl_sql_str.as_str(),
            row.status.as_sql(),
            row.deploy_id.as_str(),
            row.actor.as_sql(),
            schema_version_str.as_str(),
        ];

        let rows = self.session.query(&sql, &params).await?;
        let value = rows
            .first()
            .and_then(|row| row.first())
            .and_then(|cell| cell.as_deref())
            .ok_or_else(|| DbError::internal("sqlite audit: missing inserted id"))?;
        value.parse::<i64>().map_err(|e| {
            DbError::internal(format!("sqlite audit: invalid inserted id {value:?}: {e}"))
        })
    }

    async fn update_audit_status(
        &self,
        app_id: &str,
        id: i64,
        new_status: crate::audit::TerminalStatus,
        error: Option<&str>,
    ) -> Result<bool, DbError> {
        let q_app = self.quote_ident(app_id);
        let id_str = id.to_string();
        let status = new_status.as_sql();

        let rows = if let Some(err) = error {
            let sql = format!(
                "UPDATE {q_app}.\"__zeroship_migrations\" \
                 SET status = ?2, \
                     error = ?3, \
                     updated_at = CURRENT_TIMESTAMP, \
                     applied_at = CASE \
                         WHEN ?2 IN ('applied','applied_with_dead_letter') AND applied_at IS NULL \
                         THEN CURRENT_TIMESTAMP \
                         ELSE applied_at \
                     END \
                 WHERE id = ?1 AND status IN ('running','pending') \
                 RETURNING id"
            );
            self.session.query(&sql, &[id_str.as_str(), status, err]).await?
        } else {
            let sql = format!(
                "UPDATE {q_app}.\"__zeroship_migrations\" \
                 SET status = ?2, \
                     updated_at = CURRENT_TIMESTAMP, \
                     applied_at = CASE \
                         WHEN ?2 IN ('applied','applied_with_dead_letter') AND applied_at IS NULL \
                         THEN CURRENT_TIMESTAMP \
                         ELSE applied_at \
                     END \
                 WHERE id = ?1 AND status IN ('running','pending') \
                 RETURNING id"
            );
            self.session.query(&sql, &[id_str.as_str(), status]).await?
        };

        Ok(!rows.is_empty())
    }
}

impl DialectBuilder for SqliteBackend {
    // The backend forwards every dialect call to the `SqliteDialect`
    // ZST so consumers can hold an `&SqliteBackend` and reach the
    // dialect without naming the inner type. The ZST is instantiated
    // per call — rustc inlines the value away because every method on
    // `SqliteDialect` is `&self` and side-effect-free.

    fn sql_dialect(&self) -> crate::query::SqlDialect {
        SqliteDialect.sql_dialect()
    }

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

// P1 PR 5: `Backend` composition marker. Every sub-trait
// (`SqlExecutor`, `LockManager`, `NamespaceManager`,
// `SchemaIntrospect`, `IndexBuilder`) now carries a real (non-stub)
// impl above, and the PR-1 super-trait relaxation that dropped the
// `Client = compio_postgres::Client` pin from `Backend` cleared the
// last obstacle. The marker is the one-liner the design names —
// orchestrator paths that future PRs migrate onto a backend-agnostic
// bound (`<B: Backend>`) will pick up `SqliteBackend` via this impl
// without any further per-trait wiring.
impl Backend for SqliteBackend {}

// ---------------------------------------------------------------------------
// P3 PR 3 — `SessionMinter` impl
// ---------------------------------------------------------------------------
//
// SQLite session-minter: in-memory HMAC-SHA256 + bounded LRU
// nonce cache, no persistent state. Both methods share the
// `canonical_payload` format with PG so cross-backend equivalence
// is byte-for-byte (pinned by PR 4).
//
// The cryptography and replay-detection lives in
// `session_minter.rs`; this block is pure orchestration:
//   - `mint_session_token`: nonce gen → payload build → HMAC.
//   - `init_session`: 5 checks (expiry, actor_kind, nonce length,
//     replay, signature) in PG-matched order.

impl crate::backend::SessionMinter for SqliteBackend {
    async fn mint_session_token(
        &self,
        init: crate::backend::SessionInit,
        ttl_secs: Option<i64>,
    ) -> Result<crate::backend::MintedToken, DbError> {
        // Secret is required at mint time — lazy failure per plan
        // §11 Q-P3-H. A backend booted without
        // `ZEROSHIP_SESSION_SECRET` set still serves plain DB
        // ops; only the SessionMinter surface is degraded.
        let secret = self.minter_secret.as_ref().ok_or_else(|| DbError::Configuration {
            code: "not_configured",
            message: "SQLite SessionMinter not configured (set ZEROSHIP_SESSION_SECRET)"
                .into(),
            hint: Some(
                "Generate a 32-byte hex secret with `openssl rand -hex 32` and \
                 export it as ZEROSHIP_SESSION_SECRET."
                    .into(),
            ),
        })?;

        let ttl = ttl_secs.unwrap_or(crate::auth::util::DEFAULT_TOKEN_TTL_SECS);

        // 32-byte nonce. `/dev/urandom` preferred; the
        // time-perturbed fallback in `getrandom_or_fallback`
        // logs a warning on misses — production deployments
        // notice if entropy goes missing.
        let mut nonce = vec![0u8; 32];
        crate::auth::util::getrandom_or_fallback(&mut nonce);

        let expires_at_iso = crate::auth::util::iso_timestamp_after(ttl);

        // Canonical payload: byte-for-byte equivalent to PG's
        // `__zeroship_admin.sign_session` body. Empty
        // `actor_id` / `pid` mirror PG's `COALESCE(..., '')`
        // (actor_id) / the `init.pid.as_deref().unwrap_or("")`
        // contract for the new `pid` field.
        let payload = session_minter::canonical_payload(
            &init.actor_kind,
            init.actor_id.as_deref().unwrap_or(""),
            init.pid.as_deref().unwrap_or(""),
            &nonce,
            &expires_at_iso,
        );

        let signature = session_minter::compute_signature(secret, &payload);

        Ok(crate::backend::MintedToken {
            app_id: init.app_id,
            actor_kind: init.actor_kind,
            actor_id: init.actor_id,
            pid: init.pid,
            // SQLite has no PG-style backend PID. Always 0 per
            // the cross-backend `MintedToken` contract — PG uses
            // this for SECURITY DEFINER `p_pid`; SQLite binds
            // via `pid` instead.
            backend_pid: 0,
            nonce,
            expires_at_iso,
            signature,
        })
    }

    async fn init_session(
        &self,
        token: &crate::backend::MintedToken,
    ) -> Result<(), DbError> {
        // Secret required at verify time too. Same lazy-failure
        // contract as `mint_session_token`.
        let secret = self.minter_secret.as_ref().ok_or_else(|| DbError::Configuration {
            code: "not_configured",
            message: "SQLite SessionMinter not configured (set ZEROSHIP_SESSION_SECRET)"
                .into(),
            hint: None,
        })?;

        // -- Step 1: expiry. The signed payload includes
        // `expires_at_iso`, so any tamper would fail signature
        // verify — but we check expiry FIRST because an expired
        // legitimate token shouldn't even reach the HMAC path.
        // The 5 reject codes match PG's SECURITY DEFINER
        // `init_session` DETAIL tags 1-for-1.
        let exp_ms = session_minter::parse_iso_to_millis(&token.expires_at_iso)
            .ok_or_else(|| {
                DbError::validation(
                    "session_invalid_signature",
                    "malformed expires_at in token",
                )
            })?;
        let now_ms = session_minter::current_unix_millis();
        if exp_ms < now_ms {
            return Err(DbError::validation(
                "session_signature_expired",
                "session-init signature expired",
            ));
        }

        // -- Step 2: actor_kind allowlist. The allowlist matches
        // the PG SECURITY DEFINER `init_session` literal list
        // (`auth/bootstrap.rs::install_init_session_function`):
        // `('auto','user','operator','ai-builder','platform')`.
        const ALLOWED_ACTOR_KINDS: [&str; 5] =
            ["auto", "user", "operator", "ai-builder", "platform"];
        if !ALLOWED_ACTOR_KINDS.contains(&token.actor_kind.as_str()) {
            return Err(DbError::validation(
                "session_invalid_actor_kind",
                format!("invalid actor_kind: {}", token.actor_kind),
            ));
        }

        // -- Step 3: nonce length. PG checks
        // `octet_length(p_nonce) < 16`. The `getrandom_or_fallback`
        // path always produces 32 bytes; this guards against
        // hand-crafted forgeries with a too-short nonce.
        if token.nonce.len() < 16 {
            return Err(DbError::validation(
                "session_nonce_too_short",
                "nonce too short (need >=16 bytes)",
            ));
        }

        // -- Step 4: nonce replay. Inserted BEFORE the HMAC
        // verify so a forged token doesn't fill the cache more
        // cheaply than a real one — same ordering as the PG
        // SECURITY DEFINER (the INSERT INTO session_nonces
        // happens before the verify_signature call).
        self.nonce_cache
            .borrow_mut()
            .insert_if_fresh(&token.nonce, exp_ms)?;

        // -- Step 5: HMAC verify. Constant-time. When
        // `minter_secret_prev` is `Some(...)`, BOTH branches
        // always run — `verify_signature` is structured so the
        // wall-clock time doesn't reveal which key matched.
        let payload = session_minter::canonical_payload(
            &token.actor_kind,
            token.actor_id.as_deref().unwrap_or(""),
            token.pid.as_deref().unwrap_or(""),
            &token.nonce,
            &token.expires_at_iso,
        );
        let ok = session_minter::verify_signature(
            secret,
            self.minter_secret_prev.as_deref(),
            &payload,
            &token.signature,
        );
        if !ok {
            return Err(DbError::validation(
                "session_invalid_signature",
                "invalid session-init signature",
            ));
        }

        // SQLite has no `session_ctx` table — there is no per-PID
        // session-context concept here. Downstream audit-write
        // paths (when ported to SQLite) bind context through the
        // session actor's per-call state instead.
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// P4 PR 7 — `VectorIndex` impl (sqlite-vec `vec0` virtual table)
// ---------------------------------------------------------------------------
//
// Swapped from the pure-Rust flat scan that landed in P4 PR 4 (the
// original Q-P4-D decision in `docs/proposals/p4-search-implementation-plan.md`
// §10). Reassessment dated 2026-05-24 corrected the bundled-vs-`.so`
// mistake: the `sqlite-vec` Rust crate compiles the C extension
// statically and registers it via `sqlite3_auto_extension`
// (`session::register_sqlite_vec_once`). NO `.so` ships; the
// bundled-SQLite invariant (design §1) is preserved.
//
// Storage shape: each `t.vector(dims, { metric })` column gets a
// paired vec0 virtual table `<coll>__vec_<col>`, declared with the
// engine-native `float[<dims>]` element type + `distance_metric=`
// configuration. AFTER triggers mirror `(rowid, <col>)` into the
// vec0 table on INSERT/UPDATE/DELETE; the base table still holds the
// canonical write surface (a `BLOB` column) so the SDK's regular
// INSERT path lands writes there, the CDC preupdate hook observes
// them, and the trigger fans the row out to the vec0 index.
//
// Two methods:
//   * `ensure_vector_index` — runs five idempotent statements:
//       1. CREATE VIRTUAL TABLE IF NOT EXISTS `<coll>__vec_<col>` USING vec0(...)
//       2. Initial population INSERT INTO __vec_<col> SELECT FROM `<coll>`
//          (guarded by a sqlite_master presence probe so we only seed once)
//       3. AFTER INSERT trigger `<coll>__vec_<col>_ai`
//       4. AFTER DELETE trigger `<coll>__vec_<col>_ad`
//       5. AFTER UPDATE OF cols trigger `<coll>__vec_<col>_au`
//     Inner-product metric is rejected via [`vector::reject_inner_product`]
//     — vec0 supports cosine + L2 only.
//   * `vector_search` — emits a JOIN against the vec0 vtable on
//     rowid, MATCHes the query vector through vec0's KNN operator,
//     orders by `v.distance`, applies the filter via the standard
//     `build_find` machinery, and decodes the result rows through the
//     session actor's `query_typed` path.
//
// **Trigger-vs-preupdate-hook coexistence** (Q-P4-F): same ordering
// guarantees as FTS5 — preupdate fires BEFORE the row mutation, AFTER
// triggers fire after, both run inside the same transaction. The
// broker sees the base-row event with the vec0 index already updated
// at COMMIT time. See `fts.rs` rustdoc for the canonical walkthrough.

impl crate::backend::VectorIndex for SqliteBackend {
    /// Idempotently create the vec0 virtual table + mirror triggers
    /// for `<app>.<collection>.<column>`. Runs five statements in
    /// order: CREATE VIRTUAL TABLE, gated initial population, three
    /// AFTER triggers. The CREATE VIRTUAL TABLE / CREATE TRIGGER
    /// statements ARE idempotent via `IF NOT EXISTS`; the population
    /// step is gated by a `sqlite_master` probe so it runs exactly
    /// once at vtable-creation time.
    ///
    /// **Metric**: cosine or L2 land in the vec0 vtable declaration
    /// (`distance_metric=cosine|l2`). Inner product is not a
    /// vec0-native metric — `VectorMetric::InnerProduct` surfaces as
    /// a typed `vector_unsupported_metric` Configuration error. The
    /// SDK can branch on the wire code; PG callers continue to
    /// support all three metrics via pgvector opclasses.
    async fn ensure_vector_index(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        dims: i32,
        metric: crate::backend::VectorMetric,
    ) -> Result<(), DbError> {
        // 0. Reject inner-product up-front (vec0 only supports cosine
        //    + L2 at vtable-creation time).
        vector::reject_inner_product(metric)?;

        // 1. Probe whether the vec0 vtable already exists. If yes, we
        //    skip the initial-population INSERT (it's NOT idempotent —
        //    running it twice doubles the index payload). The CREATE
        //    VIRTUAL TABLE / CREATE TRIGGER statements ARE idempotent
        //    via `IF NOT EXISTS` so we re-run them unconditionally.
        let vec_table = vector::vec_table_name(collection, column);
        let probe_sql = format!(
            "SELECT 1 FROM {qschema}.sqlite_master \
             WHERE type = 'table' AND name = '{esc_name}'",
            qschema = SqliteDialect.quote_ident(app_id),
            // The probe's `name = '<lit>'` is a single-quoted SQL
            // literal — escape any embedded `'` by doubling. The
            // collection + column names were validated at the SDK
            // boundary.
            esc_name = vec_table.replace('\'', "''"),
        );
        let existing = self.session.query(&probe_sql, &[]).await?;
        let vtable_exists = !existing.is_empty();

        // 2. CREATE VIRTUAL TABLE IF NOT EXISTS — emits the vec0 vtable
        //    with the documented `float[N] distance_metric=...` shape.
        let create_sql =
            vector::build_create_vec0_sql(app_id, collection, column, dims, metric);
        self.session.exec(&create_sql, &[]).await?;

        // 3. Initial population — only if the vtable did NOT exist
        //    before this call. Skipping the re-population is the only
        //    reason we needed the sqlite_master probe; the rest of
        //    the DDL is idempotent.
        if !vtable_exists {
            let populate_sql =
                vector::build_initial_population_sql(app_id, collection, column);
            // `session.exec` returns the rusqlite `Connection::changes()`
            // value — we don't care about the count here, only the
            // failure path (e.g. no such base table / column). The
            // typed `DbError` propagates via `?`.
            self.session.exec(&populate_sql, &[]).await?;
        }

        // 4-6. AFTER triggers (idempotent via `IF NOT EXISTS`).
        let insert_trg = vector::build_insert_trigger_sql(app_id, collection, column);
        self.session.exec(&insert_trg, &[]).await?;
        let delete_trg = vector::build_delete_trigger_sql(app_id, collection, column);
        self.session.exec(&delete_trg, &[]).await?;
        let update_trg = vector::build_update_trigger_sql(app_id, collection, column);
        self.session.exec(&update_trg, &[]).await?;

        Ok(())
    }

    /// vec0-powered top-k vector search. Composes a SQL of the form
    ///
    /// ```sql
    /// SELECT t.*, v.distance AS _distance
    ///   FROM "<app>"."<coll>" t
    ///   JOIN "<app>"."<coll>__vec_<col>" v ON t.rowid = v.rowid
    ///  WHERE v."<col>" MATCH x'…' AND k = ?
    ///    AND <filter>
    ///  ORDER BY v.distance;
    /// ```
    ///
    /// The query vector is bound as a hex BLOB literal (`x'…'`) so we
    /// don't need a binary-bind channel through the session actor's
    /// `&[&str]` parameter surface. The byte layout is native LE
    /// `f32` — same as the `vec_f32` constructor's expected form.
    /// `k` is bound positionally; the trailing filter params (if
    /// any) follow.
    async fn vector_search(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        query: &[f32],
        k: usize,
        metric: crate::backend::VectorMetric,
        filter: &serde_json::Value,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        // Reject inner-product before issuing any SQL — vec0 vtables
        // can't be created with `distance_metric=ip`, so a stale
        // ensure_vector_index call would have failed earlier; this
        // catches a direct vector_search call (no prior ensure) that
        // asks for IP.
        vector::reject_inner_product(metric)?;

        // Build the filter WHERE clause via the shared lowering. The
        // builder emits `$N` placeholders + a parallel params Vec.
        // We use the raw `build_where` (not `build_find`) so we
        // don't have to slice a SELECT prefix off — `build_where`
        // returns the WHERE expression text directly (or an empty
        // string if `filter` is non-object / `Null`).
        let mut params: Vec<String> = Vec::new();
        let where_expr = crate::query::build_where(filter, &mut params)
            .map_err(DbError::from)?;

        // Inline the query vector as a hex BLOB literal. SQLite's
        // x'…' syntax is the canonical form for binary literals and
        // sidesteps the actor's text-only param channel.
        let query_bytes = vector::vec_to_le_bytes(query);
        let mut query_hex = String::with_capacity(query_bytes.len() * 2 + 4);
        query_hex.push_str("x'");
        for byte in &query_bytes {
            query_hex.push_str(&format!("{byte:02x}"));
        }
        query_hex.push('\'');

        // Compose the SQL. The MATCH operand uses the inline blob; k
        // is bound positionally as $1 (the first param after the
        // WHERE clause's existing params, which we'll re-number on a
        // fresh `?` placeholder — SQLite accepts `?` anonymous binds
        // alongside `$N` named ones; the params are appended in order
        // and bound positionally by rusqlite).
        //
        // **Param order**: the `where_sql` carries `$1..$M` for the
        // filter; we append the `k` literal directly into the SQL
        // (it's a small integer, safe to format) so the param vec
        // doesn't need re-numbering.
        let qschema = SqliteDialect.quote_ident(app_id);
        let qcoll = SqliteDialect.quote_ident(collection);
        let qvtab = SqliteDialect.quote_ident(&vector::vec_table_name(collection, column));
        let qcol = SqliteDialect.quote_ident(column);

        let extra_filter = if where_expr.is_empty() {
            String::new()
        } else {
            // Compose the filter with the MATCH + k conditions via
            // AND. The `build_where` lowering produces unqualified
            // column references (e.g. `"name" = $1`); they resolve
            // against the base table `t` in our JOIN. The vec0 table
            // only exposes `rowid` + the vector column + `distance`
            // + `k`, so collision risk is bounded — a user column
            // accidentally named `rowid` / `distance` / `k` would
            // shadow, but the SDK reserves `_`-prefixed names and
            // these aren't `_`-prefixed; SQLite's identifier
            // resolution prefers the first-listed table on collision
            // (which is `t`), but we explicitly qualify the vec0
            // references (`v.<col>`) to be safe.
            format!(" AND {where_expr}")
        };

        let sql = format!(
            "SELECT t.*, v.distance AS _distance \
             FROM {qschema}.{qcoll} t \
             JOIN {qschema}.{qvtab} v ON t.rowid = v.rowid \
             WHERE v.{qcol} MATCH {query_hex} AND k = {k}{extra_filter} \
             ORDER BY v.distance"
        );

        let param_refs: Vec<&str> = params.iter().map(String::as_str).collect();
        let typed = self.session.query_typed(&sql, &param_refs).await?;
        Ok(crate::v8_bridge::typed_rows_to_json_value(&typed))
    }
}

// ---------------------------------------------------------------------------
// P4 PR 5 — `FullTextIndex` impl (FTS5 external-content vtables)
// ---------------------------------------------------------------------------
//
// FTS5 ships in rusqlite's `bundled` feature by default (the SQLite
// amalgamation we link in already carries `SQLITE_ENABLE_FTS5`). No
// Cargo flag toggle, no runtime extension load.
//
// Two methods:
//   * `ensure_fts_index` — runs five idempotent statements:
//       1. CREATE VIRTUAL TABLE IF NOT EXISTS `<coll>__fts` USING fts5(...)
//       2. Initial population INSERT INTO __fts SELECT FROM `<coll>`
//          (guarded by a vtable-presence probe so we only seed once)
//       3. AFTER INSERT trigger `<coll>__fts_ai`
//       4. AFTER DELETE trigger `<coll>__fts_ad`
//       5. AFTER UPDATE OF cols trigger `<coll>__fts_au`
//     `language` is logged via `tracing::debug!` but otherwise ignored —
//     FTS5's default tokeniser is language-agnostic Unicode.
//   * `fts_search` — composes the JOIN + MATCH + filter + ORDER BY bm25
//     SQL via `fts::build_fts_search_sql`, binds (query, limit, filter
//     params) positionally, and re-emits each row as `serde_json::Value`
//     with a synthetic `_rank: f64` field.
//
// **Trigger-vs-preupdate-hook coexistence** (Q-P4-F): preupdate fires
// BEFORE the row mutation; AFTER triggers fire after. Both run within
// the same transaction — the broker sees the base-row event with the
// FTS index already updated at COMMIT time. See `fts.rs` module
// rustdoc for the canonical ordering walkthrough.

impl crate::backend::FullTextIndex for SqliteBackend {
    async fn ensure_fts_index(
        &self,
        app_id: &str,
        collection: &str,
        columns: &[String],
        language: &str,
    ) -> Result<(), DbError> {
        if columns.is_empty() {
            return Err(DbError::Configuration {
                code: "fts_no_columns",
                message: "db: ensure_fts_index requires at least one source column"
                    .to_string(),
                hint: Some(
                    "mark at least one t.string() field with `.fts()` in the schema"
                        .to_string(),
                ),
            });
        }
        // SQLite FTS5's default tokeniser is language-agnostic Unicode;
        // `language` is honoured on the PG arm but ignored here. The
        // SDK already validates the language token; log it so an
        // operator wondering why an `es` tokeniser produces the same
        // hits as `en` sees the cause in the structured log.
        tracing::debug!(
            app_id = %app_id,
            collection = %collection,
            language = %language,
            "SqliteBackend::ensure_fts_index: `language` is ignored \
             — FTS5 default tokeniser is language-agnostic Unicode"
        );

        // Probe whether the FTS vtable already exists. If it does, we
        // skip the initial-population INSERT (which is NOT idempotent
        // — running it twice doubles the index payload). The CREATE
        // VIRTUAL TABLE / CREATE TRIGGER statements ARE idempotent via
        // `IF NOT EXISTS`, so we re-run them unconditionally — cheap,
        // and it picks up any column-list drift across `registerModel`
        // calls (though changing the column list isn't supported on
        // FTS5 in-place; that's a DROP + RECREATE path the diff engine
        // handles in a future PR).
        let probe_sql = format!(
            "SELECT 1 FROM {qschema}.sqlite_master \
             WHERE type = 'table' AND name = '{coll}__fts'",
            qschema = SqliteDialect.quote_ident(app_id),
            // The probe's `name = '<lit>'` is a single-quoted SQL
            // literal — escape any embedded `'` by doubling. The
            // collection name was validated at the SDK boundary.
            coll = collection.replace('\'', "''"),
        );
        let existing = self.session.query(&probe_sql, &[]).await?;
        let vtable_exists = !existing.is_empty();

        // 1. CREATE VIRTUAL TABLE IF NOT EXISTS — emits the external-
        //    content FTS5 vtable.
        let create_sql =
            fts::build_create_fts_table_sql(app_id, collection, columns);
        self.session.exec(&create_sql, &[]).await?;

        // 2. Initial population — only if the vtable did NOT exist
        //    before this call. Skipping the re-population is the only
        //    reason we needed the sqlite_master probe; the rest of
        //    the DDL is idempotent.
        if !vtable_exists {
            let populate_sql =
                fts::build_initial_population_sql(app_id, collection, columns);
            self.session.exec(&populate_sql, &[]).await?;
        }

        // 3-5. AFTER triggers (idempotent via `IF NOT EXISTS`).
        let insert_trg =
            fts::build_insert_trigger_sql(app_id, collection, columns);
        self.session.exec(&insert_trg, &[]).await?;
        let delete_trg =
            fts::build_delete_trigger_sql(app_id, collection, columns);
        self.session.exec(&delete_trg, &[]).await?;
        let update_trg =
            fts::build_update_trigger_sql(app_id, collection, columns);
        self.session.exec(&update_trg, &[]).await?;

        Ok(())
    }

    async fn fts_search(
        &self,
        app_id: &str,
        collection: &str,
        query: &str,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        // Param layout: `[$1=query, $2=limit?, $3+...=filter_params]`.
        // When `limit` is `None` we skip the `$2` slot — the filter
        // params shift down to `$2+`. The builder is param-offset-
        // aware (it counts `params.len() + 1` for each new placeholder)
        // so seeding the pre-filter slots in order keeps the numbering
        // consistent.
        let mut params: Vec<String> = Vec::with_capacity(4);
        params.push(query.to_string());
        let has_limit = limit.is_some();
        if let Some(l) = limit {
            params.push(l.to_string());
        }

        let filter_clause = fts::build_fts_filter_clause(filter, &mut params)
            .map_err(DbError::from)?;
        let sql = fts::build_fts_search_sql(app_id, collection, &filter_clause, has_limit);

        let param_refs: Vec<&str> = params.iter().map(String::as_str).collect();
        let typed = self.session.query_typed(&sql, &param_refs).await?;
        Ok(crate::v8_bridge::typed_rows_to_json_value(&typed))
    }
}

// ---------------------------------------------------------------------------
// P4 PR 5 — `SpatialIndex` impl (pure-Rust haversine + flat scan)
// ---------------------------------------------------------------------------
//
// Pure-Rust over an R-tree (Q-P4-C, plan §4.3): same rationale as the
// vector path's pure-Rust-over-`sqlite-vec` decision — bundling the
// R-tree extension would require either forking the SQLite
// amalgamation per CI platform or runtime-loading a `.so`, both of
// which defeat the "no system libsqlite3" invariant. The haversine
// flat scan is acceptable at dev scale; production spatial workloads
// run on PostGIS via the PG arm.
//
// Two methods:
//   * `ensure_spatial_index` — no-op. Flat scan needs no index; the
//     CHECK constraint on the `geoPoint` BLOB column is emitted by
//     [`spatial::sqlite_geopoint_column_ddl`] at column-DDL time.
//   * `spatial_near` — SELECT all rows matching `filter` via the
//     session actor's `query_typed`, decode each row's `column` blob
//     via `spatial::blob_to_point`, compute `haversine_m(point, row_point)`,
//     filter rows with `distance <= radius_m`, sort ASC, take top-
//     `limit`, and re-emit as JSON with a synthetic `_distance_m: f64`
//     field.

impl crate::backend::SpatialIndex for SqliteBackend {
    async fn ensure_spatial_index(
        &self,
        _app_id: &str,
        _collection: &str,
        _column: &str,
    ) -> Result<(), DbError> {
        Ok(())
    }

    async fn spatial_near(
        &self,
        app_id: &str,
        collection: &str,
        column: &str,
        point: crate::backend::GeoPoint,
        radius_m: f64,
        filter: &serde_json::Value,
        limit: Option<usize>,
    ) -> Result<Vec<serde_json::Value>, DbError> {
        // Build the WHERE clause via the same machinery `dispatch_find`
        // uses (the SQLite-on-PG-SQL path; `$N` placeholders bind
        // positionally on rusqlite). No ORDER BY at the SQL layer —
        // we sort in Rust by computed distance.
        let bq = crate::query::build_find(
            app_id,
            collection,
            filter,
            /* limit  */ None,
            /* offset */ None,
            /* order_by */ None,
            /* select   */ None,
        )
        .map_err(DbError::from)?;
        let param_refs: Vec<&str> = bq.params.iter().map(String::as_str).collect();
        let typed = self.session.query_typed(&bq.sql, &param_refs).await?;

        // Locate the BLOB column. Cache the index outside the row
        // loop so we don't scan `columns` per row.
        let col_idx = typed
            .columns
            .iter()
            .position(|name| name == column)
            .ok_or_else(|| {
                DbError::validation(
                    "invalid_geo_arg",
                    format!(
                        "db: geoPoint column '{column}' not found in result row \
                         (have: {:?})",
                        typed.columns
                    ),
                )
            })?;

        // Compute distances. `(f64, row_idx)` keeps the sort key + a
        // back-pointer to the row; we only re-serialise rows that
        // pass the radius filter.
        let mut scored: Vec<(f64, usize)> = Vec::with_capacity(typed.rows.len());
        for (idx, row) in typed.rows.iter().enumerate() {
            let cell = row.get(col_idx).ok_or_else(|| {
                DbError::internal("spatial_near: typed row cell-count mismatch")
            })?;
            let blob_bytes: &[u8] = match cell {
                session::TypedCell::Blob(b) => b.as_slice(),
                session::TypedCell::Null => {
                    // NULL geoPoint — skip this row from the candidate
                    // set (same convention as the vector path's NULL
                    // skip). The `NOT NULL` CHECK in
                    // `sqlite_geopoint_column_ddl` keeps NULLs from
                    // landing in the column at all under canonical
                    // emission, but a hand-crafted schema might
                    // permit NULL — be defensive.
                    continue;
                }
                other => {
                    return Err(DbError::validation(
                        "invalid_geo_arg",
                        format!(
                            "db: geoPoint column '{column}' is not a BLOB (saw {:?})",
                            std::mem::discriminant(other)
                        ),
                    ));
                }
            };
            let row_point = spatial::blob_to_point(blob_bytes)?;
            let d = spatial::haversine_m(point, row_point);
            if d <= radius_m {
                scored.push((d, idx));
            }
        }
        // Sort ASC by distance. `total_cmp` handles NaN deterministically
        // (NaN sorts to the end) — haversine shouldn't produce NaN
        // for valid lat/lng but stay total.
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        if let Some(l) = limit {
            scored.truncate(l);
        }

        // Build the JSON rows through the shared typed-row decoder, then
        // append the synthetic `_distance_m` field.
        let mut out: Vec<serde_json::Value> = Vec::with_capacity(scored.len());
        for (d, idx) in scored {
            let row = &typed.rows[idx];
            let mut obj = crate::v8_bridge::typed_row_to_json_object(&typed.columns, row);
            obj.insert(
                "_distance_m".to_string(),
                serde_json::Number::from_f64(d)
                    .map_or(serde_json::Value::Null, serde_json::Value::Number),
            );
            out.push(serde_json::Value::Object(obj));
        }
        Ok(out)
    }
}

// ===========================================================================
// P5 PR 3 — Real EncryptedColumn impl on SqliteBackend
// ===========================================================================
//
// Symmetric to the PG-side impl in `backend/postgres.rs` (which landed
// in PR 2). Crypto math is shared with PG via `crate::encryption::aead`;
// key sourcing diverges: SQLite is env-var-only (`KeySource::EnvVar`)
// because there's no admin-schema sidecar (no SECURITY DEFINER getter
// equivalent on SQLite). Mirrors the session-minter pattern (P3) where
// the secret comes from `ZEROSHIP_SESSION_SECRET`.
//
// Key sourcing on SQLite is env-var-only; the `sqlite` feature gate on
// this file already restricts the build to SQLite-enabled targets.

// **P5 PR 3** — Real `EncryptedColumn` body. Delegates to the workspace
// `crate::encryption::aead` module (mode-dispatch on encrypt; mode-
// agnostic on decrypt because the wire format carries the nonce). Key
// resolution goes through `self.key_store` (env-var-only on SQLite).
impl crate::backend::EncryptedColumn for SqliteBackend {
    type KeyHandle = crate::encryption::aead::AeadKey;

    async fn resolve_key(
        &self,
        app_id: &str,
        key_id: &str,
    ) -> Result<Self::KeyHandle, DbError> {
        self.key_store.resolve(app_id, key_id).await
    }

    fn encrypt(
        &self,
        key: &Self::KeyHandle,
        mode: crate::backend::EncryptionMode,
        plaintext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError> {
        match mode {
            crate::backend::EncryptionMode::Randomised => {
                crate::encryption::aead::encrypt_randomised(key, plaintext, aad)
            }
            crate::backend::EncryptionMode::Deterministic => {
                crate::encryption::aead::encrypt_deterministic(key, plaintext, aad)
            }
        }
    }

    fn decrypt(
        &self,
        key: &Self::KeyHandle,
        _mode: crate::backend::EncryptionMode,
        ciphertext: &[u8],
        aad: &[u8],
    ) -> Result<Vec<u8>, DbError> {
        // Decrypt is mode-agnostic: the wire format carries the nonce,
        // and AES-GCM verifies the tag regardless of how the nonce was
        // produced on the write side. The caller picks the
        // mode-appropriate AAD (Camp A: row_pk in AAD for Randomised,
        // omitted for Deterministic) — see
        // `crate::crud::encryption_pass`.
        crate::encryption::aead::decrypt(key, ciphertext, aad)
    }
}

/// **P5 PR 3** — recover per-column encryption metadata from the
/// `/* zsenc:<mode>:<keyId>:<wraps> */` sentinel comments the DDL
/// emitter writes into the `CREATE TABLE` text (see
/// `crate::query::field_to_column`).
///
/// Returns a map from column name → [`crate::diff::EncryptionMeta`].
/// Columns without an attached sentinel are absent from the map (which
/// is the same shape `EncryptionMeta` round-trips through —
/// `ColumnInfo::encryption = None` for plain columns).
///
/// **Implementation note**: this is a tiny hand-rolled parser instead
/// of a `regex` dep — the workspace doesn't carry `regex` for plugin-db
/// and the sentinel format is fixed enough that a few `.split` /
/// `.find` calls cover every case the DDL emitter produces. The
/// canonical regex equivalent is
/// `/"([^"]+)"\s+\w+\s*\/\* zsenc:(randomised|deterministic):
/// ([A-Za-z0-9_]+):(string|number|bytes) \*\//` — every column DDL the
/// emitter writes for an encrypted field is of the shape
/// `"<col>" BYTEA /* zsenc:<mode>:<keyId>:<wraps> */ <constraints>`,
/// so we walk the CREATE TABLE body finding `/* zsenc:...` markers and
/// rewind to the preceding double-quoted identifier.
///
/// **Sidecar upgrade path**: regex-on-DDL is fragile — a future SDK
/// that emits column DDL with non-trivial line breaks or stacked
/// comments could trip per-column attachment. The plan §11 Q-P5 calls
/// out a sidecar `__zs_schema_meta` table as the eventual upgrade;
/// PR 3 ships the regex per the implementation plan's §5
/// trade-off acknowledgement.
fn parse_encryption_sentinels(
    create_table_text: &str,
) -> std::collections::HashMap<String, crate::diff::EncryptionMeta> {
    use crate::diff::{EncryptionMeta, WrappedType};
    let mut out = std::collections::HashMap::new();
    // Walk the body, finding each `/* zsenc:...` marker. For each one,
    // rewind to the most recent double-quoted identifier to recover the
    // column name. The emitter always emits the column name as the
    // first token in the column DDL (e.g. `"ssn" BYTEA /* zsenc:...`),
    // so the rewind is unambiguous.
    const MARKER: &str = "/* zsenc:";
    let mut search_pos = 0usize;
    while let Some(found) = create_table_text[search_pos..].find(MARKER) {
        let abs_marker = search_pos + found;
        // Find the matching `*/` after the marker.
        let body_start = abs_marker + MARKER.len();
        let Some(end_rel) = create_table_text[body_start..].find("*/") else {
            break; // unterminated comment — bail out of the walk
        };
        let body = &create_table_text[body_start..body_start + end_rel];
        let body_trim = body.trim();
        // body = "<mode>:<keyId>:<wraps>"
        let parts: Vec<&str> = body_trim.split(':').collect();
        if parts.len() == 3 {
            let mode = match parts[0] {
                "randomised" | "randomized" => Some(crate::backend::EncryptionMode::Randomised),
                "deterministic" => Some(crate::backend::EncryptionMode::Deterministic),
                _ => None,
            };
            let key_id = parts[1];
            let wraps = match parts[2] {
                "string" => Some(WrappedType::String),
                "number" => Some(WrappedType::Number),
                "bytes" => Some(WrappedType::Bytes),
                _ => None,
            };
            // Validate the key_id alphabet ([A-Za-z0-9_]) — matches
            // the SDK's `encrypted_invalid_key_id` guard. A malformed
            // key id would have been rejected at register time; here
            // we just guard against parsing garbage from a hand-edited
            // DDL.
            let key_id_ok = !key_id.is_empty()
                && key_id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_');

            if let (Some(mode), Some(wraps), true) = (mode, wraps, key_id_ok) {
                // Rewind from `abs_marker` to find the column name. The
                // column name is the most recent `"…"` token before the
                // marker — scan backwards for the closing `"` then the
                // opening `"`.
                let before = &create_table_text[..abs_marker];
                if let Some(col_name) = recover_preceding_quoted_ident(before) {
                    out.insert(
                        col_name,
                        EncryptionMeta {
                            mode,
                            key_id: key_id.to_string(),
                            wraps,
                        },
                    );
                }
            }
        }
        search_pos = body_start + end_rel + "*/".len();
    }
    out
}

/// **P5.5 PR 6** — recover per-parent-column mask metadata from the
/// `/* __zsmask:kind=…,classification=… */` sentinel comments the DDL
/// emitter writes alongside every `<col>_masked` sibling column (see
/// `crate::query::build_create_table_with_fks`).
///
/// Returns a map keyed on the **PARENT** column name (the sibling's
/// existence is the discoverability hook, but the mask metadata
/// belongs on the parent — the diff classifier compares
/// `live.parent.mask` against `declared.parent.mask`). Parents
/// without a sibling are absent from the map; the sibling's
/// existence is implicit in the sentinel attachment.
///
/// **Parse fence**: a sentinel that doesn't parse cleanly (unknown
/// kind, unknown classification, malformed body) is logged via
/// `tracing::warn!` and skipped — the parent column then reads as
/// unmasked, and a re-deploy regenerates the sentinel. This mirrors
/// the PG arm's treatment in `crate::diff::read_live_schema` so both
/// arms surface the same "loud-but-recoverable" failure shape.
///
/// Same hand-rolled walker pattern as
/// [`parse_encryption_sentinels`] — no `regex` dep required.
fn parse_mask_sentinels(
    create_table_text: &str,
) -> std::collections::HashMap<String, crate::diff::MaskMeta> {
    use crate::diff::MaskMeta;
    let mut out = std::collections::HashMap::new();
    const MARKER: &str = "/* __zsmask:";
    let mut search_pos = 0usize;
    while let Some(found) = create_table_text[search_pos..].find(MARKER) {
        let abs_marker = search_pos + found;
        // The marker swallows the leading `/* ` so the comment body
        // starts at `__zsmask:`. We find the matching `*/` to extract
        // the full sentinel payload.
        let body_start = abs_marker + "/* ".len();
        let Some(end_rel) = create_table_text[body_start..].find("*/") else {
            break;
        };
        let body = create_table_text[body_start..body_start + end_rel].trim();
        // Reuse the canonical parser so the wire shape is centralised.
        match crate::crud::mask_backfill::parse_mask_sentinel(body) {
            Ok((kind, classification)) => {
                let before = &create_table_text[..abs_marker];
                if let Some(sibling_name) = recover_preceding_quoted_ident(before) {
                    if let Some(parent) = sibling_name.strip_suffix("_masked") {
                        out.insert(
                            parent.to_string(),
                            MaskMeta {
                                kind,
                                classification,
                                sibling_column: sibling_name.clone(),
                            },
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    sentinel = %body,
                    error = %e.clone().into_string(),
                    "diff: malformed mask sentinel on SQLite sibling column; \
                     treating parent column as unmasked",
                );
            }
        }
        search_pos = body_start + end_rel + "*/".len();
    }
    out
}

/// Find the most recent double-quoted identifier in `text`, returning
/// the identifier's contents (with `""` un-escaped to `"`). Returns
/// `None` if no closing-then-opening `"` pair is found.
fn recover_preceding_quoted_ident(text: &str) -> Option<String> {
    // Scan from the right for a closing `"`, then for the matching
    // opening `"`. Handles the SQL `""` doubled-quote escape: a
    // sequence like `"foo""bar"` is one ident "foo\"bar". We do not
    // attempt full SQL parsing — the DDL emitter's column names are
    // already validated to `[A-Za-z0-9_]` via `validate_field_name`,
    // so the simple "last `"` token before the marker" rule is exact.
    let bytes = text.as_bytes();
    // Find closing `"`.
    let mut close = None;
    for i in (0..bytes.len()).rev() {
        if bytes[i] == b'"' {
            close = Some(i);
            break;
        }
    }
    let close = close?;
    // Find opening `"`. Doubled-quote escape (`""`) must be a single
    // logical quote — but our emitter validates field names to
    // `[A-Za-z0-9_]` so doubled quotes can't appear in a real column
    // name. We tolerate them defensively by skipping pairs.
    let mut open = None;
    let mut i = close;
    while i > 0 {
        i -= 1;
        if bytes[i] == b'"' {
            // Check for an escaped doubled quote: if the character
            // immediately before is ALSO `"`, this is part of an escape
            // sequence and we should continue past both.
            if i > 0 && bytes[i - 1] == b'"' {
                i -= 1;
                continue;
            }
            open = Some(i);
            break;
        }
    }
    let open = open?;
    if open + 1 >= close {
        return None;
    }
    let raw = &text[open + 1..close];
    // Un-escape doubled quotes (`""` → `"`). Field names won't contain
    // them in practice; this is defensive.
    Some(raw.replace("\"\"", "\""))
}

// ---------------------------------------------------------------------------
// P5 PR 5 — `Backup` capability (VACUUM INTO snapshot + atomic
// file-swap restore + `pitr_pg_only` refusal).
// ---------------------------------------------------------------------------
//
// Three methods on `impl Backup for SqliteBackend`:
//
//   * `snapshot(app_id, dest_uri, opts)`:
//       1. Hold the per-app `register_model` advisory lock through the
//          in-process `LockManager` so concurrent register_model /
//          migration can't reshape the schema during the copy.
//       2. Parse `dest_uri` — `file://` only (S3/HTTPS deferred,
//          mirrors the PG arm in PR 4). Bare paths accepted.
//       3. Send `Command::VacuumInto { Some(app_id), dest_path }` to
//          the session actor. The actor runs
//          `VACUUM "<app>" INTO '<dest>'` against the per-app ATTACH
//          alias on the control connection.
//       4. On `SQLITE_BUSY`, the `BusyPolicy` decides: Abort -> typed
//          `backup_busy` Coded error; Retry -> 3-attempt bounded loop
//          at 0/100/500ms.
//       5. Stream-hash the dest file with SHA-256.
//       6. Release the lock; return `SnapshotHandle { uri, content_hash,
//          created_at_ms }`.
//
//   * `restore(app_id, snapshot)`:
//       1. Hold the per-app `register_model` lock.
//       2. Resolve `snapshot.uri` to a `file://` path.
//       3. Re-hash the file and compare to `snapshot.content_hash`.
//          Mismatch -> `snapshot_hash_mismatch` Coded error, BEFORE
//          touching the live DB.
//       4. Copy the snapshot to a temp file beside the live per-app
//          DB so the atomic rename happens on the same filesystem.
//       5. Send `Command::ReattachFile { app_id, temp_path, live_path }`
//          to the actor. The actor DETACHes, renames, ATTACHes
//          sequentially.
//       6. Release the lock.
//
//   * `pitr_replay(app_id, target)`:
//       SQLite has no WAL-archive PITR. Returns
//       `Configuration { code: "pitr_pg_only" }` unconditionally.

impl crate::backend::Backup for SqliteBackend {
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: crate::backend::SnapshotOpts,
    ) -> Result<crate::backend::SnapshotHandle, DbError> {
        backup_sqlite::snapshot_impl(self, app_id, dest_uri, opts).await
    }

    async fn restore(
        &self,
        app_id: &str,
        snapshot: &crate::backend::SnapshotHandle,
    ) -> Result<(), DbError> {
        backup_sqlite::restore_impl(self, app_id, snapshot).await
    }

    async fn pitr_replay(
        &self,
        _app_id: &str,
        _target: crate::backend::PitrTarget,
    ) -> Result<(), DbError> {
        Err(DbError::Configuration {
            code: "pitr_pg_only",
            message:
                "SQLite has no WAL-archive PITR — use snapshot/restore against a \
                 per-app file copy instead. PITR replay is supported only on the \
                 PG backend (recovery_target_lsn / recovery_target_time)."
                    .into(),
            hint: Some(
                "Configure WAL archiving on a PG backend to enable PITR; for the \
                 SQLite arm use Backup::snapshot followed by Backup::restore."
                    .into(),
            ),
        })
    }
}

/// Inner module so the SQLite-side `Backup` helpers stay grouped and
/// the surrounding file keeps its "thin trait facade + per-capability
/// impl block" shape — matches the PG arm's `backup_pg` inner module
/// in `backend/postgres.rs`.
///
/// `pub(super)` so the trait methods above can call in; the helpers
/// stay private to this file.
mod backup_sqlite {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::SqliteBackend;
    use crate::backend::{BusyPolicy, LockScope, PitrTarget, SnapshotHandle, SnapshotOpts};
    use crate::error::DbError;

    /// Tag used by both `snapshot` and `restore` for the per-app
    /// register_model advisory lock. Matches the PG arm's literal
    /// `REGISTER_MODEL_LOCK_TAG = "register_model"`; the SQLite arm
    /// doesn't expose that constant outside `orchestrator/register_model`,
    /// so we duplicate the literal here. A future shared-constant lift
    /// can unify both sides.
    const REGISTER_MODEL_LOCK_TAG: &str = "register_model";

    /// Parse a `file:///abs/path` URI into the underlying filesystem
    /// path. Mirrors the PG arm's `parse_dest_path` shape so the SDK
    /// error codes stay consistent across backends.
    ///
    /// PR 5 (SQLite) supports only the `file://` scheme. Bare paths
    /// (no scheme) are also accepted so operators can pass either
    /// form. S3 / HTTPS surface as `backup_dest_uri_unsupported`.
    fn parse_dest_path(dest_uri: &str) -> Result<PathBuf, DbError> {
        if let Some(rest) = dest_uri.strip_prefix("file://") {
            Ok(PathBuf::from(rest))
        } else if dest_uri.starts_with("s3://") || dest_uri.starts_with("https://") {
            Err(DbError::Configuration {
                code: "backup_dest_uri_unsupported",
                message: format!(
                    "snapshot destination URI {dest_uri:?} uses an unsupported scheme; \
                     P5 PR 5 (SQLite) ships `file://` only — S3/HTTPS land alongside the \
                     production BlobStore wire-through in a later PR"
                ),
                hint: Some(
                    "use `file:///abs/path/to/snapshot.sqlite` in P5 PR 5; \
                     S3/R2 destinations are deferred"
                        .to_string(),
                ),
            })
        } else {
            Ok(PathBuf::from(dest_uri))
        }
    }

    fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }

    /// Stream `path` through SHA-256 and return the 32-byte digest.
    /// Reads in 64 KiB chunks — same shape as the PG arm.
    fn sha256_file(path: &Path) -> Result<[u8; 32], std::io::Error> {
        use sha2::Digest;
        let mut file = std::fs::File::open(path)?;
        let mut hasher = sha2::Sha256::new();
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(hasher.finalize().into())
    }

    /// Acquire the per-app `register_model` advisory lock through the
    /// in-process registry. Returns the `(key1, key2)` pair so the
    /// caller can release it symmetrically. On contention emits the
    /// typed `migration_in_progress` Coded error (mirrors the PG arm).
    async fn acquire_register_model_lock(
        backend: &SqliteBackend,
        app_id: &str,
        op: &'static str,
    ) -> Result<(String, String), DbError> {
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: REGISTER_MODEL_LOCK_TAG.to_string(),
        };
        let (k1, k2) = scope.to_keys();
        // Use a bounded poll matching the PG arm's
        // `try_acquire_with_backoff` schedule (5 attempts, 0/50/200/500/1000ms,
        // ~1.75s budget).
        const SCHEDULE: &[u64] = &[0, 50, 200, 500, 1000];
        for &pre_wait in SCHEDULE {
            if pre_wait > 0 {
                compio::time::sleep(std::time::Duration::from_millis(pre_wait)).await;
            }
            if backend
                .lock_registry
                .try_acquire((k1.clone(), k2.clone()))
            {
                return Ok((k1, k2));
            }
        }
        Err(DbError::Coded {
            code: "migration_in_progress".to_string(),
            message: format!(
                "{op}: another deploy / migration is in progress for app {app_id:?} \
                 (in-process register_model lock held; 5-attempt bounded retry exhausted)"
            ),
            hint: Some(
                "retry the operation once the in-flight register_model / migration \
                 completes"
                    .to_string(),
            ),
        })
    }

    /// Drop the register_model lock acquired by [`acquire_register_model_lock`].
    /// Infallible at the registry layer — unheld slots emit a
    /// `tracing::warn` no-op. Matches the contract of every other
    /// `release_advisory_lock` site.
    fn release_register_model_lock(backend: &SqliteBackend, k1: String, k2: String) {
        backend.lock_registry.release((k1, k2));
    }

    /// Classify a session-level `DbError` from `VACUUM INTO` as the
    /// `SQLITE_BUSY`-equivalent retryable error. SQLite's
    /// `busy_timeout=5000` PRAGMA absorbs most contention internally;
    /// surfacing here means a schema-change race or checkpointer
    /// holding the exclusive lock past the timeout. The
    /// `error::from_sqlite` classifier maps `SQLITE_BUSY` to
    /// `DbError::LockContention` (per the existing classifier shape).
    fn is_busy_error(e: &DbError) -> bool {
        matches!(e, DbError::LockContention { .. })
    }

    pub(super) async fn snapshot_impl(
        backend: &SqliteBackend,
        app_id: &str,
        dest_uri: &str,
        opts: SnapshotOpts,
    ) -> Result<SnapshotHandle, DbError> {
        // 1. Hold the per-app register_model lock for the whole
        //    snapshot. Concurrent migrations would otherwise reshape
        //    schema mid-copy; the in-process registry serialises every
        //    register_model call against this same scope.
        let (k1, k2) = acquire_register_model_lock(backend, app_id, "snapshot").await?;

        // 2. Parse + prepare destination.
        let dest_path = match parse_dest_path(dest_uri) {
            Ok(p) => p,
            Err(e) => {
                release_register_model_lock(backend, k1, k2);
                return Err(e);
            }
        };
        if let Some(parent) = dest_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    release_register_model_lock(backend, k1, k2);
                    return Err(DbError::Internal {
                        message: format!(
                            "snapshot: create parent dir {parent:?} failed: {e}"
                        ),
                    });
                }
            }
        }
        // Refuse if the dest path already exists — SQLite refuses to
        // VACUUM INTO an existing file. We surface this as a typed
        // Configuration error so the operator's tool gets a clean
        // signal rather than the raw rusqlite "output file already
        // exists" message.
        if dest_path.exists() {
            release_register_model_lock(backend, k1, k2);
            return Err(DbError::Configuration {
                code: "backup_dest_exists",
                message: format!(
                    "snapshot: destination {dest_path:?} already exists — SQLite \
                     VACUUM INTO refuses to overwrite existing files"
                ),
                hint: Some(
                    "remove the existing file or choose a different destination path \
                     before re-running the snapshot"
                        .to_string(),
                ),
            });
        }

        let dest_path_str = dest_path.to_string_lossy().into_owned();

        // 3. Issue VACUUM INTO via the session actor. The actor body
        //    `run_vacuum_into` constructs the literal-quoted SQL and
        //    runs `VACUUM "<app>" INTO '<dest>'` against the per-app
        //    ATTACH alias on the control connection (which is where
        //    the ensure_app_schema path attached the per-app file).
        //
        //    Busy-policy retry: 3 attempts at 0/100/500ms when
        //    `opts.if_busy == Retry`. SQLite's bootstrap PRAGMA
        //    `busy_timeout=5000` absorbs most contention internally so
        //    a surfaced LockContention here is rare; the retry caps
        //    additional wait at ~0.6s on top of the PRAGMA budget.
        let attempts: &[u64] = match opts.if_busy {
            BusyPolicy::Abort => &[0],
            BusyPolicy::Retry => &[0, 100, 500],
        };
        let mut last_err: Option<DbError> = None;
        for &pre_wait in attempts {
            if pre_wait > 0 {
                compio::time::sleep(std::time::Duration::from_millis(pre_wait)).await;
            }
            match backend
                .session
                .vacuum_into(Some(app_id), &dest_path_str)
                .await
            {
                Ok(()) => {
                    last_err = None;
                    break;
                }
                Err(e) if is_busy_error(&e) => {
                    last_err = Some(e);
                    continue;
                }
                Err(e) => {
                    release_register_model_lock(backend, k1, k2);
                    // Clean up a partial dest file so a re-run doesn't
                    // see stale bytes.
                    let _ = std::fs::remove_file(&dest_path);
                    return Err(e);
                }
            }
        }
        if let Some(e) = last_err {
            // Exhausted retries (or Abort with one failed attempt) on
            // a SQLITE_BUSY-equivalent. Translate to the typed
            // `backup_busy` Coded code the SDK can branch on.
            release_register_model_lock(backend, k1, k2);
            let _ = std::fs::remove_file(&dest_path);
            return Err(DbError::Coded {
                code: "backup_busy".to_string(),
                message: format!(
                    "snapshot: SQLite busy after {} attempt(s) — {e}",
                    attempts.len()
                ),
                hint: match opts.if_busy {
                    BusyPolicy::Retry => Some(
                        "transient — re-run the snapshot once the contending \
                         schema-change / checkpointer releases"
                            .to_string(),
                    ),
                    BusyPolicy::Abort => Some(
                        "pass SnapshotOpts { if_busy: BusyPolicy::Retry } to absorb \
                         transient busy events with a bounded backoff"
                            .to_string(),
                    ),
                },
            });
        }

        // 4. Compute the content hash over the persisted file. Runs
        //    blocking std-fs reads on the compio thread (same as the
        //    PG arm); the snapshot path is operator-driven and not hot
        //    enough to warrant spawn_blocking.
        let content_hash = match sha256_file(&dest_path) {
            Ok(h) => h,
            Err(e) => {
                release_register_model_lock(backend, k1, k2);
                let _ = std::fs::remove_file(&dest_path);
                return Err(DbError::Internal {
                    message: format!(
                        "snapshot: SHA-256 of {dest_path_str:?} failed: {e}"
                    ),
                });
            }
        };

        // 5. Release the lock now that the snapshot is committed to
        //    disk. From here on concurrent register_model can proceed;
        //    the SnapshotHandle's content_hash pins integrity for the
        //    eventual restore.
        release_register_model_lock(backend, k1, k2);

        Ok(SnapshotHandle {
            uri: dest_uri.to_string(),
            content_hash,
            created_at_ms: now_ms(),
        })
    }

    pub(super) async fn restore_impl(
        backend: &SqliteBackend,
        app_id: &str,
        snapshot: &SnapshotHandle,
    ) -> Result<(), DbError> {
        // 1. Hold the per-app register_model lock for the whole
        //    restore. Without it, a concurrent register_model would
        //    race the DETACH/rename/ATTACH sequence.
        let (k1, k2) =
            acquire_register_model_lock(backend, app_id, "restore").await?;

        // 2. Resolve the snapshot URI to an on-disk path. PR 5
        //    supports file:// only.
        let src_path = match parse_dest_path(&snapshot.uri) {
            Ok(p) => p,
            Err(e) => {
                release_register_model_lock(backend, k1, k2);
                return Err(e);
            }
        };

        // 3. Re-hash and verify integrity BEFORE touching the live
        //    DB. A mismatch means the snapshot was tampered with or
        //    truncated; refuse before any DETACH/rename.
        match sha256_file(&src_path) {
            Ok(observed) if observed == snapshot.content_hash => { /* ok */ }
            Ok(_) => {
                release_register_model_lock(backend, k1, k2);
                return Err(DbError::Coded {
                    code: "snapshot_hash_mismatch".to_string(),
                    message: format!(
                        "restore: SHA-256 of {src_path:?} does not match the \
                         SnapshotHandle's recorded hash — snapshot is corrupt or \
                         this is the wrong file"
                    ),
                    hint: Some(
                        "re-fetch the snapshot from the original source; do NOT \
                         run restore against a file whose hash has drifted"
                            .to_string(),
                    ),
                });
            }
            Err(e) => {
                release_register_model_lock(backend, k1, k2);
                return Err(DbError::Internal {
                    message: format!(
                        "restore: SHA-256 of {src_path:?} failed: {e}"
                    ),
                });
            }
        }

        // 4. Copy the snapshot into a sibling temp file beside the
        //    live per-app DB so the atomic rename happens on the same
        //    filesystem. POSIX `rename` is atomic only when both paths
        //    are on one FS; the plan documents this caveat as the
        //    operator contract. A direct `rename(src_path → live)`
        //    would consume the operator-supplied snapshot file (which
        //    they may want to keep) AND fail across filesystems.
        let live_path = backend.db_dir.join(format!("zs-{app_id}.sqlite"));
        let temp_path = backend.db_dir.join(format!("zs-{app_id}.sqlite.restore-tmp"));
        // Best-effort cleanup of a stale tmp from a prior crashed run.
        let _ = std::fs::remove_file(&temp_path);
        if let Err(e) = std::fs::copy(&src_path, &temp_path) {
            release_register_model_lock(backend, k1, k2);
            return Err(DbError::Internal {
                message: format!(
                    "restore: std::fs::copy({src_path:?} -> {temp_path:?}) failed: {e}; \
                     ensure the snapshot destination shares a filesystem with the live \
                     per-app DB directory ({:?})",
                    backend.db_dir
                ),
            });
        }

        // 5. Signal the session actor to DETACH the current live
        //    file, atomically rename temp → live, and re-ATTACH the
        //    alias against the new content. The actor's
        //    `run_reattach_file` runs the three steps on the worker
        //    thread; the CDC hook triplet on the control connection
        //    stays armed across the swap because hooks are bound to
        //    `sqlite3*`, not to an attached DB.
        let temp_path_str = temp_path.to_string_lossy().into_owned();
        let live_path_str = live_path.to_string_lossy().into_owned();
        if let Err(e) = backend
            .session
            .reattach_file(app_id, &temp_path_str, &live_path_str)
            .await
        {
            release_register_model_lock(backend, k1, k2);
            // Best-effort: leave the temp file in place so the
            // operator can inspect it; do NOT delete on error.
            return Err(e);
        }

        // 6. Restore complete. Release the lock.
        release_register_model_lock(backend, k1, k2);
        Ok(())
    }

    /// `pitr_replay` does not need a helper — the impl method body
    /// returns the typed `Configuration { code: "pitr_pg_only" }`
    /// directly. PG retains its own replay path (PR 4); SQLite cannot
    /// participate without a WAL-archive substrate.
    #[allow(dead_code)]
    fn _pitr_marker(_target: PitrTarget) {}
}

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
        AuditWriter, Backend, DialectBuilder, IndexBuilder, LockManager, NamespaceManager,
        SchemaIntrospect, SqlExecutor,
    };

    #[test]
    fn memory_backend_tempdir_is_removed_on_drop() {
        let runtime = compio::runtime::Runtime::new().expect("compio runtime");
        let temp_dir_path = runtime.block_on(async {
            let backend = SqliteBackend::open(":memory:")
                .await
                .expect("open in-memory backend");
            let temp_dir_path = backend.db_dir().to_path_buf();
            assert!(temp_dir_path.exists(), "temp dir should exist while backend lives");
            drop(backend);
            temp_dir_path
        });
        assert!(
            !temp_dir_path.exists(),
            "TempDir-backed SQLite scratch dir should be cleaned on drop"
        );
    }

    /// P1 PR 5: `Backend` composition marker now lands on
    /// `SqliteBackend`. Pinning the bound here means a future change
    /// that detaches one of the five sub-trait impls (or that
    /// regresses the PR-1 super-bound relaxation back to
    /// `Client = compio_postgres::Client`) fails compilation in this
    /// module rather than at a distant orchestrator call site.
    fn assert_sqlite_backend_impls_backend() {
        fn assert_impl<T: Backend>() {}
        assert_impl::<SqliteBackend>();
    }

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

    /// P1 PR 5: `AuditWriter` capability — pin the impl wire so a
    /// future refactor that detaches the trait-impl block from this
    /// type fails compilation here, not at the `IndexBuilder`
    /// consumer site that pulls the audit row through.
    fn assert_sqlite_backend_impls_audit_writer() {
        fn assert_impl<T: AuditWriter>() {}
        assert_impl::<SqliteBackend>();
    }

    /// P3 PR 3: `SessionMinter` capability — pin the SQLite-arm
    /// impl wire so a future refactor that detaches the trait-impl
    /// block fails compilation here.
    fn assert_sqlite_backend_impls_session_minter() {
        fn assert_impl<T: crate::backend::SessionMinter>() {}
        assert_impl::<SqliteBackend>();
    }

    /// P4 PR 4: `VectorIndex` capability — pin the SQLite-arm impl
    /// wire so the pure-Rust flat-scan path's trait composition
    /// regresses at compile time if the impl block is detached or
    /// the method shape drifts from the trait surface.
    fn assert_sqlite_backend_impls_vector_index() {
        fn assert_impl<T: crate::backend::VectorIndex>() {}
        assert_impl::<SqliteBackend>();
    }

    /// P4 PR 5: `FullTextIndex` capability — pin the SQLite-arm impl
    /// wire so the FTS5 vtable + AFTER-trigger path's trait composition
    /// regresses at compile time if the impl block is detached.
    fn assert_sqlite_backend_impls_full_text_index() {
        fn assert_impl<T: crate::backend::FullTextIndex>() {}
        assert_impl::<SqliteBackend>();
    }

    /// P4 PR 5: `SpatialIndex` capability — pin the SQLite-arm impl
    /// wire so the haversine flat-scan path's trait composition
    /// regresses at compile time if the impl block is detached.
    fn assert_sqlite_backend_impls_spatial_index() {
        fn assert_impl<T: crate::backend::SpatialIndex>() {}
        assert_impl::<SqliteBackend>();
    }

    /// P2 PR 1: pin the SQLite-arm [`ChangeStream`] adapter
    /// (`crate::backend::sqlite::cdc::SqliteChangeStream`) with the
    /// agreed `ConsumerHandle = SqliteConsumerHandle` shape. A
    /// regression that detaches the impl block — or renames the
    /// associated type — trips here, not at the
    /// `BackendHandle::as_change_stream_sqlite()` accessor.
    fn assert_sqlite_change_stream_impls_change_stream() {
        use crate::backend::ChangeStream;
        use crate::backend::sqlite::cdc::{SqliteChangeStream, SqliteConsumerHandle};
        fn assert_impl<T: ChangeStream<ConsumerHandle = SqliteConsumerHandle>>() {}
        assert_impl::<SqliteChangeStream>();
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

    // -----------------------------------------------------------------
    // P5 PR 3 — encryption sentinel parser unit tests
    // -----------------------------------------------------------------

    /// Round-trip a single encrypted column: emitter shape → parser
    /// extracts `(mode, key_id, wraps)` correctly.
    #[test]
    fn parse_encryption_sentinel_single_column() {
        let ddl = "CREATE TABLE \"app\".\"users\" (\n  \
            id SERIAL PRIMARY KEY,\n  \
            \"ssn\" BYTEA /* zsenc:randomised:default:string */  NOT NULL,\n  \
            \"name\" TEXT \n)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("ssn").expect("ssn must be parsed");
        assert!(matches!(m.mode, crate::backend::EncryptionMode::Randomised));
        assert_eq!(m.key_id, "default");
        assert!(matches!(m.wraps, crate::diff::WrappedType::String));
        assert!(got.get("name").is_none(), "non-encrypted col must be absent");
        assert!(got.get("id").is_none());
    }

    /// Deterministic mode + non-string wraps + custom key id.
    #[test]
    fn parse_encryption_sentinel_deterministic_number_custom_key() {
        let ddl = "CREATE TABLE \"app\".\"events\" (\n  \
            \"salary\" BYTEA /* zsenc:deterministic:payroll_v2:number */ NOT NULL\n)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("salary").expect("salary must be parsed");
        assert!(matches!(
            m.mode,
            crate::backend::EncryptionMode::Deterministic
        ));
        assert_eq!(m.key_id, "payroll_v2");
        assert!(matches!(m.wraps, crate::diff::WrappedType::Number));
    }

    /// US spelling `randomized` round-trips as canonical Randomised
    /// (the DDL emitter normalises but a hand-edited DDL could carry
    /// the US form).
    #[test]
    fn parse_encryption_sentinel_accepts_us_spelling() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zsenc:randomized:default:bytes */)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("a").expect("a must be parsed");
        assert!(matches!(m.mode, crate::backend::EncryptionMode::Randomised));
        assert!(matches!(m.wraps, crate::diff::WrappedType::Bytes));
    }

    /// Multiple encrypted columns in one CREATE TABLE — each attaches
    /// to its own column name.
    #[test]
    fn parse_encryption_sentinel_multiple_columns() {
        let ddl = "CREATE TABLE \"app\".\"u\" (\n  \
            \"ssn\" BYTEA /* zsenc:randomised:default:string */,\n  \
            \"tin\" BYTEA /* zsenc:deterministic:tax:string */\n)";
        let got = parse_encryption_sentinels(ddl);
        assert_eq!(got.len(), 2);
        assert!(matches!(
            got["ssn"].mode,
            crate::backend::EncryptionMode::Randomised
        ));
        assert!(matches!(
            got["tin"].mode,
            crate::backend::EncryptionMode::Deterministic
        ));
        assert_eq!(got["tin"].key_id, "tax");
    }

    /// Malformed sentinel — wrong number of parts → ignored (the
    /// column ends up without metadata; no panic).
    #[test]
    fn parse_encryption_sentinel_rejects_malformed() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zsenc:only_one_part */)";
        let got = parse_encryption_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// Unknown mode / wraps → ignored.
    #[test]
    fn parse_encryption_sentinel_rejects_unknown_mode() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zsenc:hashed:default:string */)";
        let got = parse_encryption_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// Invalid key_id alphabet → ignored.
    #[test]
    fn parse_encryption_sentinel_rejects_invalid_key_id() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zsenc:randomised:bad key:string */)";
        let got = parse_encryption_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// DDL with no sentinels → empty map (no allocations beyond the
    /// HashMap itself).
    #[test]
    fn parse_encryption_sentinel_empty_when_no_marker() {
        let ddl = "CREATE TABLE t (\"a\" TEXT, \"b\" INTEGER)";
        let got = parse_encryption_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// Unterminated comment doesn't loop forever; we bail out.
    #[test]
    fn parse_encryption_sentinel_handles_unterminated_comment() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zsenc:randomised:default:string";
        let got = parse_encryption_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// `recover_preceding_quoted_ident` finds the most recent quoted
    /// token before the marker position.
    #[test]
    fn recover_preceding_quoted_ident_picks_last_token() {
        let text = "CREATE TABLE \"app\".\"users\" ( \"ssn\" BYTEA ";
        assert_eq!(
            recover_preceding_quoted_ident(text).as_deref(),
            Some("ssn")
        );
    }

    #[test]
    fn recover_preceding_quoted_ident_handles_empty() {
        assert!(recover_preceding_quoted_ident("").is_none());
        assert!(recover_preceding_quoted_ident("no quotes here").is_none());
    }

    // -----------------------------------------------------------------
    // P5.5 PR 6 — mask sentinel parser
    // -----------------------------------------------------------------

    /// **PR 6 — SQLite introspection**: a CREATE TABLE body with an
    /// inline `/* __zsmask:kind=…,classification=… */` comment attached
    /// to the `<col>_masked` sibling column gets parsed back as a
    /// `MaskMeta` on the PARENT column.
    #[test]
    fn sqlite_introspection_reads_mask_sentinel_in_create_sql() {
        use crate::diff::{Classification, MaskKind};
        let ddl = "CREATE TABLE \"app\".\"users\" (\n  \
            \"id\" INTEGER PRIMARY KEY,\n  \
            \"ssn\" TEXT,\n  \
            \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=last4,classification=spi */\n)";
        let got = parse_mask_sentinels(ddl);
        let meta = got.get("ssn").expect("mask meta on parent");
        assert_eq!(meta.kind, MaskKind::Last4);
        assert_eq!(meta.classification, Classification::Spi);
        assert_eq!(meta.sibling_column, "ssn_masked");
    }

    /// Multiple masked columns in one table → one entry per parent.
    #[test]
    fn sqlite_introspection_multiple_masked_columns() {
        use crate::diff::{Classification, MaskKind};
        let ddl = "CREATE TABLE t (\n  \
            \"ssn\" TEXT,\n  \
            \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=last4,classification=spi */,\n  \
            \"email\" TEXT,\n  \
            \"email_masked\" TEXT NOT NULL /* __zsmask:kind=email,classification=pii */\n)";
        let got = parse_mask_sentinels(ddl);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("ssn").unwrap().kind, MaskKind::Last4);
        assert_eq!(got.get("ssn").unwrap().classification, Classification::Spi);
        assert_eq!(got.get("email").unwrap().kind, MaskKind::Email);
        assert_eq!(got.get("email").unwrap().classification, Classification::Pii);
    }

    /// Sentinel on a non-`_masked`-suffixed column is silently
    /// ignored — the parent recovery requires the sibling name to end
    /// in `_masked` (the platform invariant).
    #[test]
    fn sqlite_introspection_ignores_non_sibling_sentinel() {
        let ddl =
            "CREATE TABLE t (\n  \"ssn\" TEXT /* __zsmask:kind=last4,classification=spi */\n)";
        let got = parse_mask_sentinels(ddl);
        assert!(got.is_empty(), "non-sibling sentinel must not stamp parent: {got:?}");
    }

    /// Empty DDL / no markers → empty map.
    #[test]
    fn sqlite_introspection_no_markers_yields_empty_map() {
        let ddl = "CREATE TABLE t (\"a\" TEXT, \"b\" INTEGER)";
        let got = parse_mask_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// Malformed sentinel (unknown kind) → skipped + warn, parent
    /// stays unmasked.
    #[test]
    fn sqlite_introspection_malformed_sentinel_skipped() {
        let ddl =
            "CREATE TABLE t (\n  \"ssn\" TEXT,\n  \
             \"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=cosmic,classification=pii */\n)";
        let got = parse_mask_sentinels(ddl);
        assert!(
            got.is_empty(),
            "malformed sentinel must NOT stamp parent: {got:?}"
        );
    }

    /// Unterminated mask comment doesn't loop forever; we bail out.
    #[test]
    fn sqlite_introspection_unterminated_comment() {
        let ddl =
            "CREATE TABLE t (\"ssn_masked\" TEXT NOT NULL /* __zsmask:kind=last4,classification=spi";
        let got = parse_mask_sentinels(ddl);
        assert!(got.is_empty());
    }

    #[test]
    fn compile_time_trait_assertions_link() {
        // Keep the asserter functions live — same convention as the
        // PG-side `compile_time_assertions_link`.
        let _ = assert_sqlite_backend_impls_backend as fn();
        let _ = assert_sqlite_backend_impls_sql_executor as fn();
        let _ = assert_sqlite_backend_impls_lock_manager as fn();
        let _ = assert_sqlite_backend_impls_namespace_manager as fn();
        let _ = assert_sqlite_backend_impls_schema_introspect as fn();
        let _ = assert_sqlite_backend_impls_index_builder as fn();
        let _ = assert_sqlite_backend_impls_dialect_builder as fn();
        let _ = assert_sqlite_backend_impls_audit_writer as fn();
        let _ = assert_sqlite_backend_impls_session_minter as fn();
        let _ = assert_sqlite_backend_impls_vector_index as fn();
        let _ = assert_sqlite_backend_impls_full_text_index as fn();
        let _ = assert_sqlite_backend_impls_spatial_index as fn();
        let _ = assert_sqlite_change_stream_impls_change_stream as fn();
        let _ = assert_sqlite_backend_is_static as fn();
        let _ = assert_sqlite_client_pinned_to_session_handle as fn();
    }
}
