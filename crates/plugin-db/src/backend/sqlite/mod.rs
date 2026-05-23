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
use std::path::PathBuf;
use std::rc::Rc;

use serde_json::Value;

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
pub(crate) mod lock;
pub(crate) mod session;
// **P4 PR 4** — pure-Rust vector primitives (`bytemuck` round-trip +
// the three distance functions). The `impl VectorIndex for
// SqliteBackend` block at the bottom of this file routes the
// flat-scan path through this module; the SQL composition stays here
// so the cryptography-of-math stays unit-testable in `vector.rs`.
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
        // CDC packet channel — worker thread (producer, via commit
        // hook) → compio publisher task (consumer, calls
        // broker::publish on this thread).
        let (packet_tx, packet_rx) = flume::unbounded::<CommitPacket>();

        // Open the session WITH the packet sender so the worker
        // thread arms the hook triplet during PRAGMA bootstrap. The
        // `app_id` argument is currently unused inside the dispatcher
        // (per-event app_id derives from the hook's `db_name`
        // parameter — see `cdc::install` rustdoc), so we pass `None`.
        let session_path = db_dir.join("zs-control.sqlite");
        let session = Rc::new(SqliteSession::open(
            &session_path,
            None,
            Some(packet_tx),
        )?);

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

        Ok(Self {
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
            _publisher,
            minter_secret,
            minter_secret_prev,
            nonce_cache,
        })
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

        Ok(Self {
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
            _publisher,
            minter_secret: Some(secret),
            minter_secret_prev: secret_prev,
            nonce_cache,
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
        let rows = self.session.query(&sql, &[]).await?;
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
        // Build the CREATE INDEX SQL ourselves rather than reuse
        // `spec.sql` because `IndexSpec::sql` was built against the PG
        // dialect (`CREATE INDEX CONCURRENTLY` + qualified
        // `"app"."idx" ON "app"."collection" (cols)`). SQLite uses
        // `IF NOT EXISTS` (atomic, no CONCURRENTLY) and the index +
        // table identifiers route through `self.quote_ident` (the
        // dialect hook). We assemble the column list manually because
        // SQLite has no `USING <method>` clause — every index is a
        // B-tree on the listed columns.
        let q_app = self.quote_ident(app_id);
        let q_coll = self.quote_ident(collection);
        let q_idx = self.quote_ident(&spec.name);
        let cols_quoted: Vec<String> =
            spec.columns.iter().map(|c| self.quote_ident(c)).collect();
        let col_list = cols_quoted.join(", ");
        let unique_kw = if spec.unique { "UNIQUE " } else { "" };
        let sql = format!(
            "CREATE {unique_kw}INDEX IF NOT EXISTS {q_app}.{q_idx} ON {q_coll} ({col_list})"
        );

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

// P1 PR 5: `AuditWriter` capability. The SQLite impl routes the
// parameterised INSERT through the session actor. The audit table on
// SQLite is named `__zs_migrations` (the per-app analogue of PG's
// `__zeroship_migrations`); PR 5 ships only the INSERT path — the
// audit-table provisioning ddl + the `update_audit_status` transition
// path are SQLite-side work for a later PR, since `IndexBuilder` is
// the only PR-5 consumer and it writes a terminal row in one shot.
impl AuditWriter for SqliteBackend {
    async fn write_audit_row(
        &self,
        app_id: &str,
        row: &crate::audit::AuditRow,
    ) -> Result<(), DbError> {
        // Parameterised INSERT mirroring the PG-side
        // `crate::audit::write_audit_row` shape (audit.rs:333). The
        // SQLite column set is a subset (no `applied_at`, no
        // `parent_id` — those land when the SQLite audit-table
        // provisioning DDL ships). The INSERT uses `?N` positional
        // binds so the session actor's `&[&str]` param surface routes
        // through `rusqlite::Statement::execute` cleanly.
        let q_app = self.quote_ident(app_id);
        let sql = format!(
            "INSERT INTO {q_app}.\"__zs_migrations\" \
                (collection, phase, change_class, change_kind, details, \
                 ddl_sql, status, deploy_id, applied_by_kind, schema_version) \
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)"
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

        self.session.exec(&sql, &params).await?;
        Ok(())
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
// P4 PR 4 — `VectorIndex` impl (pure-Rust flat scan + bytemuck)
// ---------------------------------------------------------------------------
//
// Pure-Rust over `sqlite-vec` per the riskiest-decision Q-P4-D (plan
// §10). The flat scan is acceptable at dev scale (≤50k rows, ≤1024
// dims, ≤100ms latency); production vector workloads use pgvector
// on the PG arm. Storage layout: `BLOB` column packed as native
// little-endian `[f32]` via `bytemuck::cast_slice`; CHECK constraint
// at column-DDL time enforces `length("col") = 4 * <dims>` so the
// engine rejects wrong-dim INSERTs before they reach Rust.
//
// Two methods:
//   * `ensure_vector_index` — no-op. Flat scan needs no index
//     structure; the CHECK constraint is emitted by
//     [`crate::backend::sqlite::vector::sqlite_vector_column_ddl`] at
//     column-DDL time instead.
//   * `vector_search` — SELECT all rows matching `filter` via the
//     session actor's typed `query_typed`, decode each row's BLOB
//     column via `vector::vec_from_blob`, compute the requested
//     distance against the query vector, sort ASC, take top-k, and
//     re-emit the rows as `serde_json::Value` with an extra
//     `_distance: f32` synthetic field.

impl crate::backend::VectorIndex for SqliteBackend {
    /// SQLite flat scan needs no index structure — returns `Ok(())`
    /// immediately. The dimension contract is enforced at column-DDL
    /// time via the CHECK constraint `length("col") = 4 * <dims>` (see
    /// [`vector::sqlite_vector_column_ddl`]); the metric is consumed
    /// only by `vector_search`, where the per-row distance function
    /// dispatches on it directly.
    ///
    /// The arguments are intentionally accepted (vs. an underscored
    /// signature) so the trait shape stays uniform across backends —
    /// a future PR that introduces an actual index structure (e.g.
    /// an HNSW persistence file in `db_dir`) repoints THIS method
    /// without re-shaping callers.
    async fn ensure_vector_index(
        &self,
        _app_id: &str,
        _collection: &str,
        _column: &str,
        _dims: i32,
        _metric: crate::backend::VectorMetric,
    ) -> Result<(), DbError> {
        Ok(())
    }

    /// Flat-scan vector search. Reads every row matching `filter` via
    /// the session actor's `query_typed`, decodes the BLOB column
    /// into `Vec<f32>` via [`vector::vec_from_blob`], computes the
    /// distance against `query` per `metric`, sorts ASC, and returns
    /// the top-`k` rows as `serde_json::Value`s with a synthetic
    /// `"_distance"` field appended.
    ///
    /// **Filter composition**: routes through
    /// [`crate::query::build_find`] (the SQLite-on-PG-SQL path —
    /// SQLite supports `$N` numeric-named bind parameters with
    /// positional binding, see SQLite docs on parameters). The
    /// builder doesn't know about the BLOB column at the SQL layer;
    /// we read all matching rows and post-filter by distance in Rust
    /// (acceptable at dev scale per plan §10).
    ///
    /// **Dimension enforcement**: every row's BLOB length is checked
    /// against `query.len()` by `vec_from_blob`. A mismatch surfaces
    /// as a typed `DbError::ValidationFailed { code:
    /// "dimension_mismatch", ... }` — the same wire `.code` the SDK
    /// already branches on for pgvector.
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
        // Build the WHERE clause via the same machinery `dispatch_find`
        // uses. The builder emits `$N` numeric-named placeholders —
        // SQLite + rusqlite handle these via positional bind by index
        // (each `$N` becomes named-param N; positional binding fills
        // index 1..M from the params slice in order). No ORDER BY at
        // the SQL layer — we sort in Rust by computed distance.
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
                    "invalid_vector_arg",
                    format!(
                        "db: vector column '{column}' not found in result row \
                         (have: {:?})",
                        typed.columns
                    ),
                )
            })?;

        let expected_dims: i32 = query
            .len()
            .try_into()
            .map_err(|_| DbError::internal(
                "vector_search: query vector dim exceeds i32 range",
            ))?;

        // Compute distances. `(f32, row_idx)` so we don't pay to
        // re-serialise unselected rows; the top-k pick walks the
        // distance array.
        let mut scored: Vec<(f32, usize)> = Vec::with_capacity(typed.rows.len());
        for (idx, row) in typed.rows.iter().enumerate() {
            let cell = row.get(col_idx).ok_or_else(|| {
                DbError::internal(
                    "vector_search: typed row cell-count mismatch",
                )
            })?;
            let blob_bytes: &[u8] = match cell {
                session::TypedCell::Blob(b) => b.as_slice(),
                session::TypedCell::Null => {
                    // NULL vector — skip this row from the candidate
                    // set. pgvector behaves the same (`<=>` returns
                    // NULL for a NULL operand, and `ORDER BY` sorts
                    // NULLs to the END by default — they never reach
                    // top-k of a non-empty result).
                    continue;
                }
                other => {
                    return Err(DbError::validation(
                        "invalid_vector_arg",
                        format!(
                            "db: vector column '{column}' is not a BLOB (saw {:?})",
                            std::mem::discriminant(other)
                        ),
                    ));
                }
            };
            let vec = vector::vec_from_blob(blob_bytes, expected_dims)?;
            let d = vector::distance(metric, query, &vec);
            scored.push((d, idx));
        }
        // Sort ASC by distance. `total_cmp` handles NaN deterministically
        // (NaN sorts to the end), unlike `partial_cmp` which would
        // require an `expect`.
        scored.sort_by(|a, b| a.0.total_cmp(&b.0));
        scored.truncate(k);

        // Build the JSON rows. Each row carries every selected column
        // PLUS a synthetic `_distance` field (per the `VectorIndex`
        // trait doc-comment at backend/mod.rs:1129).
        let mut out: Vec<serde_json::Value> = Vec::with_capacity(scored.len());
        for (d, idx) in scored {
            let row = &typed.rows[idx];
            let mut obj = serde_json::Map::with_capacity(typed.columns.len() + 1);
            for (i, name) in typed.columns.iter().enumerate() {
                let cell = &row[i];
                let val = match cell {
                    session::TypedCell::Null => serde_json::Value::Null,
                    session::TypedCell::Integer(n) => {
                        serde_json::Value::Number(serde_json::Number::from(*n))
                    }
                    session::TypedCell::Real(f) => serde_json::Number::from_f64(*f)
                        .map_or(serde_json::Value::Null, serde_json::Value::Number),
                    session::TypedCell::Text(s) => serde_json::Value::String(s.clone()),
                    session::TypedCell::Blob(b) => {
                        // BLOB cells (vector and otherwise) surface as
                        // an array of byte integers — the SDK only
                        // needs the vector column for distance ranking,
                        // and the BLOB-as-string surface (`format!(<N
                        // bytes blob>)`) the untyped path uses would
                        // be unhelpful here. Future PRs may switch to
                        // base64; today the SDK's `t.vector()` type
                        // contract documents the vector column as
                        // engine-internal at the row-out boundary.
                        serde_json::Value::Array(
                            b.iter()
                                .map(|byte| {
                                    serde_json::Value::Number(serde_json::Number::from(*byte))
                                })
                                .collect(),
                        )
                    }
                };
                obj.insert(name.clone(), val);
            }
            obj.insert(
                "_distance".to_string(),
                serde_json::Number::from_f64(f64::from(d))
                    .map_or(serde_json::Value::Null, serde_json::Value::Number),
            );
            out.push(serde_json::Value::Object(obj));
        }
        Ok(out)
    }
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
        let _ = assert_sqlite_change_stream_impls_change_stream as fn();
        let _ = assert_sqlite_backend_is_static as fn();
        let _ = assert_sqlite_client_pinned_to_session_handle as fn();
    }
}
