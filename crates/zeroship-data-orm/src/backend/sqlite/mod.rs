//! SQLite adapter with owned actor sessions, native values, and catalog access.
//!
//! The driver contract implementation lives in `driver`. Session reservations
//! isolate transaction callbacks; the actor publishes committed change events
//! through the supplied change sink. Migrations own the physical schema.

use std::cell::RefCell;
use std::collections::HashSet;
use std::path::Path;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Arc;

#[cfg(any(test, feature = "test-helpers"))]
use zeroship_data_sql::value::Value;

use zeroship_data_orm::error::DbError;
use zeroship_data_orm::storage::LockManager;

use crate::cdc::ChangeSink;


/// A `ChangeSink` that drops every event, for this crate's own tests.
///
/// The adapter injects `BrokerChangeSink`; a vendor crate cannot reach it
/// without depending on the crate that composes it. Tests here exercise the
/// backend, not delivery, so a no-op port is the honest double - and it keeps
/// the composer's shape visible: `open`/`new` take the sink and the key source
/// as parameters precisely so the policy lives above.
#[cfg(test)]
struct NullChangeSink;

#[cfg(test)]
impl ChangeSink for NullChangeSink {
    fn disposition(&self, _app_id: &str) -> crate::cdc::DeliveryDisposition {
        crate::cdc::DeliveryDisposition::Deliver
    }
    fn publish(&self, _event: &zeroship_data_orm::cdc::ChangeEvent) {}
}
// `cdc` is the home for the SQLite-side `ChangeStream` adapter (the
// `preupdate_hook` install + worker->compio publisher integration).
// Crate-private - the public consumer surface is
// `BackendHandle::as_change_stream_sqlite()` (mirroring the
// `as_postgres` / `as_sqlite` accessor shape).
pub mod cdc;
pub mod error;
mod json;
pub mod lock;
/// SQLite typed-row -> JSON decoding, beside the `TypedCell`/`TypedRows` it
/// reads. Peer of `backend::pg_row_json`; see that module for why the two are
/// deliberately not shared.
pub mod row_json;
// SC-2's reservation / cancellation / terminal-classification protocol.
// Public because the cancellation surface (`SqliteCancelHandle`,
// `TerminalOutcome`) is the contract a deadline or a dropped caller-side
// future acts through; the actor in `session` is its only driver.
pub mod reservation;
// Unconditionally `pub` since the crate split: this module is now a crate
// boundary rather than a private child, so `pub(crate)` would hide it from the
// adapter that dispatches into it. The `test-helpers` arm existed to let
// `crates/zeroship-data-v8/tests/sqlite_integration.rs` name `session::TypedCell`; that need is now met
// by the boundary itself.
pub mod session;
// Pure-Rust haversine + `(lat, lng)` BLOB round-trip. The
// `impl SpatialIndex for SqliteBackend` block at the bottom of
// this file routes the flat-scan path through this module; the math
// (`haversine_m`) and the `point_to_blob` / `blob_to_point` helpers
// stay unit-testable in `spatial.rs`.
pub mod spatial;
// `sqlite-vec` vec0 vtable lifecycle + MATCH query composition.
// Supersedes the earlier pure-Rust flat scan (see
// `docs/archive/p4-search-implementation-plan.md` §10, 2026-05-24
// reassessment). The `impl VectorIndex for SqliteBackend` block at
// the bottom of this file orchestrates the five idempotent DDL
// statements + the JOIN+MATCH search path; the SQL primitives
// (`build_create_vec0_sql`, `build_*_trigger_sql`, `vec_to_le_bytes`)
// live in `vector.rs` so the documented shapes stay unit-testable
// in isolation.
pub mod vector;
// `session_minter` was the SQLite half of the HMAC session anchor. Its PG half
// was deleted on 2026-08-27 under AGENTS.md's "privilege follows the PROCESS"
// invariant; this half survived behind `test-helpers`, unreachable by any
// shipped or dev binary, and was deleted on 2026-09-02 for the same reason.
// A token the worker mints and then verifies against a secret the worker holds
// is not a boundary whichever engine stores the nonce.
// `fk_parse` was lifted out of this cfg-gated subtree so it would compile on a
// PG-only build, became `crate::backend::sqlite::cross_app_fk`, and was DELETED on 2026-09-02:
// compiled unconditionally, called from nowhere a request reaches. What keeps
// FKs inside an app is the migration engine's `validate_ident` refusing a
// dot-qualified name, plus the per-connection alias fence below. The design
// lineage
// (SQLite ATTACH file isolation per design section 18 Q1) is documented
// there too.

use cdc::CommitPacket;
use lock::InProcessLockRegistry;
use session::{SqliteSession, SqliteSessionHandle};

/// SQLite backend handle. One instance per worker thread (mirrors
/// `zeroship_data_orm::backend::postgres::PostgresBackend`'s lifecycle).
///
/// **Field set** (`docs/archive/p1-sqlite-implementation-plan.md` §2.1):
///
/// - `session`: the writer-actor handle. Owns the single
///   `rusqlite::Connection` for this backend and serialises all DDL
///   / DML / DQL through a `flume` mpsc queue.
/// - `lock_registry`: in-process advisory-lock map.
/// - `db_dir`: filesystem directory holding per-app SQLite files
///   (`zs-<app_id>.sqlite`).
/// - `app_id_cache`: dedup set for `attach_app_file`
///   path — SQLite errors on a second ATTACH of the same alias, so
///   we filter the second call site in Rust.
/// - `_publisher`: the worker->compio publisher task that
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
    cdc_name_cache_invalidations: Rc<RefCell<HashSet<(String, String)>>>,
    db_dir: PathBuf,
    app_id_cache: RefCell<HashSet<String>>,
    /// Keeps the publisher task alive for the lifetime of the
    /// backend; dropped via `Drop` when the backend goes away. The
    /// `JoinHandle` is a `compio::runtime::Task<Result<(), …>>` whose
    /// `Drop` cancels the task per the `async-task` contract (see
    /// `async_task::Task` rustdoc).
    _publisher: compio::runtime::JoinHandle<()>,
    /// Per-backend column-key cache. Resolves
    /// `(app_id, key_id) -> AeadKey` via the `ZEROSHIP_COLUMN_KEY_<KEYID>`
    /// env var (the only sourcing variant on the SQLite arm - no
    /// admin-schema sidecar; mirrors the session-minter pattern).
    /// Single-threaded (`RefCell` inside `KeyStore`) since every
    /// `SqliteBackend` is owned by a single compio thread.
    key_store: zeroship_data_orm::encryption::KeyStore,
}

fn validate_database_path(path: &Path) -> Result<(), DbError> {
    if zeroship_core::db_url::valid_sqlite_file_path(&path.to_string_lossy()) {
        Ok(())
    } else {
        Err(DbError::config(
            "sqlite_file_required",
            "SQLite requires a filesystem path; memory databases and URI options are unsupported",
        ))
    }
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
    /// Mark one table's cached CDC column-name list stale. The
    /// publisher loop clears the entry before decoding the next event
    /// for the same `(app_id, collection)` pair.
    ///
    /// UNWIRED PRODUCER, LIVE CONSUMER - do not delete it as dead. Nothing
    /// calls this today, so `cdc_name_cache_invalidations` is a set that is
    /// drained and never filled. The CONSUMER is real:
    /// `cdc::publisher_loop` does `invalidations.borrow_mut().remove(&key)`
    /// and evicts `name_cache` on a hit, so removing this leaves the
    /// publisher serving stale column names after DDL with no way to be told.
    /// The missing caller is the SQLite apply path
    /// (`docs/archive/proposals/2026-06-20-sqlite-engine-production-wiring-design.md`
    /// §7b.4: invalidate per changed collection after CreateTable/AddColumn).
    /// Deleting half a live mechanism is a design change, not a dead-code
    /// sweep; a 2026-09-04 audit flagged this as dead on caller count alone
    /// and it was kept for exactly that reason.
    #[allow(dead_code)] // producer unwired; the consumer above is not - read the doc
    pub(crate) fn invalidate_cdc_name_cache(&self, app_id: &str, collection: &str) {
        self.cdc_name_cache_invalidations
            .borrow_mut()
            .insert((app_id.to_string(), collection.to_string()));
    }

    pub async fn query_values(
        &self,
        sql: &str,
        params: &[zeroship_data_sql::value::Value],
    ) -> Result<Vec<zeroship_data_sql::value::Value>, DbError> {
        let typed = self.session.query_typed(sql, params).await?;
        crate::backend::sqlite::row_json::typed_rows_to_values(&typed)
    }

    /// Production constructor used by the runtime URL-scheme
    /// dispatcher.
    ///
    /// `path` names the control database file for the backend. Per-app
    /// files still live beside it as `zs-<app_id>.sqlite` and are bound into
    /// the session by `attach_app_file`. It is idempotent, and data-plane
    /// entry points call it lazily before addressing an app table.
    ///
    /// If `path` points at an existing directory we place the control
    /// session at `<dir>/zs-control.sqlite`. SQLite requires a filesystem path;
    /// no ephemeral database mode or implicit temporary directory is supported.
    ///
    /// `sink` is an `Arc<dyn ChangeSink>` rather than a generic because it is
    /// held on BOTH sides of the CDC channel: the commit hook on the writer
    /// thread samples `disposition`, the publisher task on the compio thread
    /// calls `publish`. One shared owner is the honest shape for that, and it
    /// keeps the generic off six signatures in this file.
    pub async fn open(
        path: impl AsRef<Path>,
        sink: Arc<dyn ChangeSink>,
        key_source: zeroship_data_orm::encryption::LocalKeySource,
    ) -> Result<Self, DbError> {
        let path = path.as_ref().to_path_buf();
        let sink_for_session = Arc::clone(&sink);
        let opened =
            compio::runtime::spawn_blocking(move || Self::open_blocking(path, sink_for_session))
                .await
                .map_err(|_| {
                    DbError::internal("SqliteBackend::open: spawn_blocking task panicked")
                })??;
        Ok(Self::finish_open(opened, sink, key_source))
    }

    // `pause_broker_for_tests` and `engage_schema_pending_for_tests` were here
    // until 2026-09-02. They forwarded to guard constructors without reading
    // any backend state, which is what made the guards look like a vendor
    // concern. Tests now call `broker::BrokerPauseGuard::new(app_id)` and
    // `broker::SchemaPendingGuard::new(app_id)` directly - there was never a
    // backend to dispatch on.
    #[allow(dead_code)]
    pub fn new(
        db_dir: PathBuf,
        sink: Arc<dyn ChangeSink>,
        key_source: zeroship_data_orm::encryption::LocalKeySource,
    ) -> Result<Self, DbError> {
        let session_path = db_dir.join("zs-control.sqlite");
        Self::open_with_session_path(db_dir, session_path, sink, key_source)
    }

    fn open_blocking(path: PathBuf, sink: Arc<dyn ChangeSink>) -> Result<OpenedBackend, DbError> {
        validate_database_path(&path)?;
        let (db_dir, session_path) = if path.is_dir() {
            let session_path = path.join("zs-control.sqlite");
            (path, session_path)
        } else {
            let db_dir = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new("."))
                .to_path_buf();
            (db_dir, path)
        };

        std::fs::create_dir_all(&db_dir).map_err(|e| {
            DbError::internal(format!(
                "SqliteBackend::open: failed to create SQLite directory {}: {e}",
                db_dir.display()
            ))
        })?;

        Self::open_session(db_dir, session_path, sink)
    }

    fn open_with_session_path(
        db_dir: PathBuf,
        session_path: PathBuf,
        sink: Arc<dyn ChangeSink>,
        key_source: zeroship_data_orm::encryption::LocalKeySource,
    ) -> Result<Self, DbError> {
        let opened = Self::open_session(db_dir, session_path, Arc::clone(&sink))?;
        Ok(Self::finish_open(opened, sink, key_source))
    }

    fn open_session(
        db_dir: PathBuf,
        session_path: PathBuf,
        sink: Arc<dyn ChangeSink>,
    ) -> Result<OpenedBackend, DbError> {
        // CDC packet channel — worker thread (producer, via commit
        // hook) → compio publisher task (consumer, calls its ChangeSink on
        // this thread).
        let (packet_tx, packet_rx) = flume::unbounded::<CommitPacket>();

        // Open the session WITH the `CommitSender` so the worker thread arms
        // the hook triplet during PRAGMA bootstrap. The sender carries the sink
        // as well as the channel because the commit hook samples
        // `ChangeSink::disposition` before it enqueues — the commit boundary is
        // where a suppression window is decided (see `cdc`'s module rustdoc).
        // The `app_id` argument is currently unused inside the dispatcher
        // (per-event app_id derives from the hook's `db_name`
        // parameter — see `cdc::install` rustdoc), so we pass `None`.
        let session = SqliteSession::open(
            &session_path,
            None,
            Some(cdc::CommitSender::new(packet_tx, sink)),
        )?;

        Ok(OpenedBackend {
            session,
            db_dir,
            packet_rx,
        })
    }

    /// `key_source` is a parameter rather than a
    /// `crate::backend::sqlite::context::isolate_key_source()` lookup, for the same tier reason
    /// the Postgres constructor takes one: the context is ENGINE state, and
    /// once this subtree is `zeroship-data-sqlite` the vendor cannot name the
    /// crate that depends on it. `crate::backend::sqlite::backend_selection` does the lookup.
    fn finish_open(
        opened: OpenedBackend,
        sink: Arc<dyn ChangeSink>,
        key_source: zeroship_data_orm::encryption::LocalKeySource,
    ) -> Self {
        let OpenedBackend {
            session,
            db_dir,
            packet_rx,
        } = opened;
        let session = Rc::new(session);
        let cdc_name_cache_invalidations = Rc::new(RefCell::new(HashSet::new()));

        // Spawn the publisher task on the current compio runtime. The
        // task captures `Rc<SqliteSession>` (for lazy column-name
        // resolution via `PRAGMA table_info`) + the receiver end of
        // the CDC channel. Dropping the returned `JoinHandle` cancels
        // the task; the channel sender on the worker thread will then
        // fail-fast on the next commit attempt (logged + dropped, no
        // commit veto).
        let _publisher = cdc::spawn_publisher(
            session.clone(),
            Rc::clone(&cdc_name_cache_invalidations),
            packet_rx,
            sink,
        );

        // Wire the column-key store. SQLite has no admin-schema sidecar
        // (no SECURITY DEFINER getter equivalent), so sourcing is always
        // LOCAL: the roots this isolate was handed, else
        // `ZEROSHIP_COLUMN_KEY_<KEYID>`. Cache lives for the lifetime of the
        // backend; clears on backend drop.
        let key_store = zeroship_data_orm::encryption::KeyStore::new(key_source);

        Self {
            session,
            lock_registry: Rc::new(InProcessLockRegistry::new()),
            cdc_name_cache_invalidations,
            db_dir,
            app_id_cache: RefCell::new(HashSet::new()),
            _publisher,
            key_store,
        }
    }
}

struct OpenedBackend {
    session: SqliteSession,
    db_dir: PathBuf,
    packet_rx: flume::Receiver<CommitPacket>,
}

// ---------------------------------------------------------------------------
// Capability impls - five carved capability blocks, per
// `p1-sqlite-implementation-plan.md` §9. The order below mirrors
// `backend/postgres.rs` so a reviewer can diff the two files
// side-by-side as the SQLite side grows.
// ---------------------------------------------------------------------------

impl SqliteBackend {
    /// A session handle bound to no reservation: every command it issues mints
    /// its own autocommit reservation and runs on `op_conn`.
    ///
    /// This is what a caller that just wants to *read* should hold.
    /// [`DatabaseFixture::fixture_session`] is the transaction lane and is
    /// exclusive - taking it for a read would serialise that read behind any
    /// open creator transaction, which is exactly the coupling SC-2 Decision 1
    /// removes.
    ///
    /// The integration target also holds one of these as a probe on `op_conn`
    /// while a transaction owns `tx_conn` - the state Decision 1 exists to make
    /// representable. That needs no gate: this is `pub` for everyone.
    pub fn autocommit_client(&self) -> SqliteSessionHandle {
        SqliteSessionHandle::new(self.session.clone())
    }

    /// **Test-only**: a transaction-lane handle the actor never bound. See
    /// [`session::SqliteSession::unregistered_transaction_handle_for_tests`].
    #[cfg(feature = "test-helpers")]
    pub fn unregistered_transaction_client_for_tests(&self) -> SqliteSessionHandle {
        self.session.unregistered_transaction_handle_for_tests()
    }

    /// **Test-only**: stall the next command *this backend's* session runs. See
    /// [`session::SqliteSession::arm_next_command_gate_for_tests`]; the gate is
    /// per-session, so it cannot be tripped by another backend's traffic.
    #[cfg(any(test, feature = "test-helpers"))]
    pub fn arm_next_command_gate_for_tests(&self) -> session::NextCommandGate {
        self.session.arm_next_command_gate_for_tests()
    }

    /// **Test-only**: an autocommit reservation the caller keeps, so it can be
    /// submitted more than once.
    ///
    /// Production mints one per command and never holds on to it, which is
    /// exactly why the op-lane ownership rule needs a helper to be testable at
    /// all: there is no production path that constructs the stale reservation
    /// the rule exists to refuse.
    #[cfg(feature = "test-helpers")]
    pub fn spent_autocommit_reservation_for_tests(
        &self,
    ) -> std::sync::Arc<crate::backend::sqlite::reservation::Reservation> {
        self.session.autocommit_reservation_for_tests()
    }

    /// **Test-only**: run one `Exec` under a caller-held reservation.
    #[cfg(feature = "test-helpers")]
    pub async fn exec_on_reservation_for_tests(
        &self,
        reservation: &std::sync::Arc<crate::backend::sqlite::reservation::Reservation>,
        sql: &str,
        params: &[&str],
    ) -> Result<u64, DbError> {
        self.session.exec_on(reservation, sql, params).await
    }

    /// **Test-only**: run a transaction handle's terminal statement and return
    /// the classified outcome. Production reaches this through
    /// `transaction::driver::terminal`, which projects the classified outcome
    /// onto SC-1's `TerminalResult`.
    #[cfg(feature = "test-helpers")]
    pub async fn settle_transaction_for_tests(
        &self,
        client: &SqliteSessionHandle,
        intent: session::TerminalIntent,
    ) -> Result<crate::backend::sqlite::reservation::TerminalOutcome, DbError> {
        client.settle(intent).await
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl crate::fixtures::DatabaseFixture for SqliteBackend {
    type Client = SqliteSessionHandle;

    async fn fixture_session(&self, app_id: &str) -> Result<Self::Client, DbError> {
        let lease = self.session.reserve_transaction(app_id).await?;
        Ok(SqliteSessionHandle::with_lease(self.session.clone(), lease))
    }

    async fn execute_fixture(&self, sql: &str, params: &[Value]) -> Result<u64, DbError> {
        self.autocommit_client().exec_values(sql, params).await
    }

    async fn execute_fixture_on(
        &self,
        client: &Self::Client,
        sql: &str,
        params: &[Value],
    ) -> Result<u64, DbError> {
        client.exec_values(sql, params).await
    }
}

impl LockManager for SqliteBackend {
    type Client = SqliteSessionHandle;
    // The default-impl methods (`acquire`, `try_acquire`, `release`,
    // `try_acquire_with_backoff`) inherit through the trait. The three
    // legacy string-key primitives below route through
    // `InProcessLockRegistry`:
    //
    // - `acquire_advisory_lock`: per plan §3.3, this method is
    //   essentially unused in production - every typed `acquire` call
    //   site routes through `try_acquire_with_backoff`.
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

impl SqliteBackend {
    /// Bind the app's file into THIS session, under the `<app_id>` alias.
    ///
    /// RENAMED from a capability-trait method, and the rename is the point:
    /// nothing here provisions a namespace. On PostgreSQL that trait method was
    /// `CREATE SCHEMA IF NOT EXISTS`; here it is an `ATTACH DATABASE`, which is
    /// session-scoped wiring, not schema management. plugin-db does not manage
    /// schema on either dialect - a migration process does - so the trait that
    /// made these two look like one operation is deleted, and the PostgreSQL
    /// half went with it (it had no production caller at all).
    ///
    /// Still idempotent, still cached by `app_id_cache`, and still tolerant of
    /// a concurrent attacher's "already attached".
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
    /// `audit::validate_app_id` before any consumer reaches
    /// `attach_app_file`, so the suffix is always UTF-8 safe. If
    /// `db_dir` itself contains non-UTF-8 bytes (rare on the Linux
    /// targets we ship to), `to_string_lossy` substitutes U+FFFD —
    /// SQLite then fails to open the resulting path and surfaces a
    /// typed `DbError` on the next call. The lossy conversion is
    /// load-bearing for the actor's `String`-typed `db_path`
    /// parameter; round-tripping through OsStr would mean carrying
    /// raw bytes across an `async` boundary the actor's reply channel
    /// already serialises as `String`.
    pub async fn attach_app_file(&self, app_id: &str) -> Result<(), DbError> {
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
        // single quotes are doubled). The ATTACH is spelled ONLY there:
        // a dialect-level template for it used to exist alongside, with
        // no consumer, because the actor needs the file_path substituted
        // upstream anyway.
        match self.session.attach(app_id, &path_str).await {
            Ok(()) => {
                self.app_id_cache.borrow_mut().insert(app_id.to_string());
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
                    self.app_id_cache.borrow_mut().insert(app_id.to_string());
                    Ok(())
                } else {
                    Err(e)
                }
            }
        }
    }
}

// `Backend` composition marker. Every sub-trait
// (`DatabaseFixture`, `LockManager`,
// `Catalog`) now carries a real (non-stub)
// impl above, and the super-trait relaxation that dropped the
// `Client = compio_postgres::Client` pin from `Backend` cleared the
// last obstacle. The marker is the one-liner the design names -
// orchestrator paths that migrate onto a backend-agnostic
// bound (`<B: Backend>`) will pick up `SqliteBackend` via this impl
// without any further per-trait wiring.
// The marker impl is NOT here: `Backend` is `zeroship-data-v8`'s own
// `pub(crate)` composition trait, so the orphan rule puts the impl in the crate
// that owns the trait even though the type is this one's. See
// `zeroship-data-v8/src/backend/mod.rs`, beside the PostgreSQL arm's.

// ---------------------------------------------------------------------------
// `VectorIndex` impl (sqlite-vec `vec0` virtual table)
// ---------------------------------------------------------------------------
//
// Swapped from the earlier pure-Rust flat scan (the
// original Q-P4-D decision in `docs/archive/p4-search-implementation-plan.md`
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
// One method: `vector_search` — emits a JOIN against the vec0 vtable on
// rowid, MATCHes the query vector through vec0's KNN operator, orders by
// `v.distance`, applies the filter via the standard `build_find`
// machinery, and decodes the result rows through the session actor's
// `query_typed` path. Inner-product is rejected up front via
// [`vector::reject_inner_product`] — vec0 supports cosine + L2 only.
//
// **This arm does NOT create the vec0 vtable or its mirror triggers, and
// nothing else in the tree does either.** `ensure_vector_index` used to,
// behind `#[cfg(any(test, feature = "test-helpers"))]`, so it never ran
// in a shipped binary; it is deleted rather than kept as data-plane DDL.
// The runtime descriptor NAMES the shadow relation and its three triggers
// (`AuxiliaryObject::ShadowTable`, `zeroship-migrate-core/src/render/
// gen_types.rs:219` and `auxiliary_objects` at `:255`), which the JOIN
// below relies on, but the engine emits no DDL for it: the SQLite renderer
// folds a vector field's index to a plain B-tree
// (`zeroship-migrate-sqlite/src/schema.rs:98`), and the engine states outright
// that it never authors a virtual table
// (`zeroship-migrate-backend/src/error.rs:270`). So `vector_search` on SQLite
// fails with "no such table" against any database the migration engine
// produced. That gap is REAL and PRE-EXISTING.
//
// One thing the engine DOES get right about it already: on a vec0 vtable it
// finds live but undeclared, the drop pass fails closed with
// `DropOfVirtualTable` (`error.rs:291`) rather than cascading the shadow
// tables away. So authoring the relation is the only half still missing - a
// migration that creates it will not be undone by the next diff.
//
// **Trigger-vs-preupdate-hook coexistence** (Q-P4-F): preupdate fires
// BEFORE the row mutation, AFTER triggers fire after, both run inside
// the same transaction. The broker sees the base-row event with the
// vec0 index already updated at COMMIT time.

impl SqliteBackend {
    /// Borrow this isolate's column-encryption key store.
    ///
    /// All that remains of the `EncryptedColumn` impl deleted on 2026-09-02;
    /// see the twin on `PostgresBackend`. Both bodies were identical, which is
    /// what made the trait a vendor coupling with no vendor content.
    pub fn key_store(&self) -> &zeroship_data_orm::encryption::KeyStore {
        &self.key_store
    }
}

/// Recover per-column encryption metadata from the
/// `/* zero-migrate:enc:<keyId>:<wraps> */` sentinel comments the DDL
/// emitter writes into the `CREATE TABLE` text (see
/// `zeroship_data_sql::compile::field_to_column`).
///
/// Returns a map from column name → [`zeroship_data_sql::catalog::EncryptionMeta`].
/// Columns without an attached sentinel are absent from the map (which
/// is the same shape `EncryptionMeta` round-trips through —
/// `ColumnInfo::encryption = None` for plain columns).
///
/// **Implementation note**: this walker is hand-rolled instead of a `regex`
/// dep — the workspace doesn't carry `regex` for plugin-db, and finding the
/// comment markers is a couple of `.find` calls. It walks the CREATE TABLE
/// body for `/* zero-migrate:enc:...` markers and rewinds to the preceding
/// double-quoted identifier, because every column DDL the emitter writes for
/// an encrypted field is of the shape
/// `"<col>" BYTEA /* zero-migrate:enc:<keyId>:<wraps> */ <constraints>`.
///
/// **The SENTINEL BODY is not parsed here.** Locating the comment is this
/// function's job; interpreting it belongs to
/// [`zeroship_data_sql::mask_codec::parse_encryption_sentinel`], the one
/// authority on the wire shape (shared with the PG introspector and the
/// migration backend). Hand-parsing it here made a third opinion of it, and
/// the third opinion drifted: it enforced a `[A-Za-z0-9_]` keyId alphabet the
/// codec does not, and dropped every mismatch in silence.
///
/// **Sidecar upgrade path**: recovering metadata from DDL TEXT is fragile — a
/// future SDK that emits column DDL with non-trivial line breaks or stacked
/// comments could trip per-column attachment. The plan §11 Q-P5 calls
/// out a sidecar `__zs_schema_meta` table as the eventual upgrade;
/// the text walk ships per the implementation plan's §5
/// trade-off acknowledgement.
fn parse_encryption_sentinels(
    create_table_text: &str,
) -> std::collections::HashMap<String, zeroship_data_sql::catalog::EncryptionMeta> {
    let mut out = std::collections::HashMap::new();
    // Walk the body, finding each `/* zero-migrate:enc:...` marker. For each one,
    // rewind to the most recent double-quoted identifier to recover the
    // column name. The emitter always emits the column name as the
    // first token in the column DDL (e.g. `"ssn" BYTEA /* zero-migrate:enc:...`),
    // so the rewind is unambiguous.
    // Composed from the shared prefix rather than spelled here: this walker
    // DISPATCHES on the marker and then hands the whole body - prefix included -
    // to the codec, so a literal that drifts from the codec's prefix does not
    // fail to parse, it finds nothing, and a column reads back unencrypted.
    let marker = format!("/* {}", zeroship_data_sql::mask_codec::ENC_SENTINEL_PREFIX);
    let marker = marker.as_str();
    let mut search_pos = 0usize;
    while let Some(found) = create_table_text[search_pos..].find(marker) {
        let abs_marker = search_pos + found;
        // The marker swallows the leading `/* `, so the comment body starts at
        // `zero-migrate:enc:` - the exact string the codec expects. Find the
        // matching `*/` to bound it.
        let body_start = abs_marker + "/* ".len();
        let Some(end_rel) = create_table_text[body_start..].find("*/") else {
            // Unterminated comment. ABORT the walk, and abort it LOUDLY - this
            // is the walk's only early exit, and a silent one is
            // indistinguishable from a table that carried no sentinels at all,
            // which is the fail-open shape every other arm here was made loud
            // to avoid.
            //
            // Aborting rather than skipping past the marker costs nothing. The
            // `find` above scans to the END of the whole text, not to the end
            // of this column's DDL, so `None` means no `*/` exists anywhere at
            // or after `body_start`. Every later marker starts later still, so
            // its own terminator search is over a subset of this one and must
            // also come back `None`. Continuing would therefore emit one
            // warning per remaining marker and recover exactly nothing.
            //
            // Reaching this at all means the DDL did not come from a CREATE
            // TABLE SQLite accepted: an unterminated `/*` inside a statement
            // body makes SQLite refuse the whole statement with "incomplete
            // input" (measured against sqlite3 3.51.2), so `sqlite_master.sql`
            // cannot hold one. An emitter bug or a hand-edited database is the
            // only way in. That it is rare is the argument for one loud line,
            // not for silence.
            let truncated = create_table_text[body_start..].trim();
            tracing::warn!(
                sentinel = %truncated,
                "diff: unterminated encryption sentinel comment in the CREATE TABLE text; \
                 abandoning the sentinel walk for this table",
            );
            break;
        };
        let body = create_table_text[body_start..body_start + end_rel].trim();
        // Reuse the canonical parser so the wire shape is centralised, and so a
        // sentinel this crate cannot interpret produces the codec's typed error
        // rather than a silent absence. Structured exactly like the mask
        // sibling below, for the same reason: both failure arms are LOUD.
        match zeroship_data_sql::mask_codec::parse_encryption_sentinel(body) {
            Ok(meta) => {
                // Rewind from `abs_marker` to find the column name. The
                // column name is the most recent `"…"` token before the
                // marker — scan backwards for the closing `"` then the
                // opening `"`.
                let before = &create_table_text[..abs_marker];
                match recover_preceding_quoted_ident(before) {
                    Some(col_name) => {
                        out.insert(col_name, meta);
                    }
                    None => {
                        tracing::warn!(
                            sentinel = %body,
                            "diff: encryption sentinel with no recoverable column name \
                             in the CREATE TABLE text; ignoring",
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    sentinel = %body,
                    error = %e,
                    "diff: malformed encryption sentinel on SQLite column; \
                     treating the column as unencrypted",
                );
            }
        }
        search_pos = body_start + end_rel + "*/".len();
    }
    out
}

/// Recover per-parent-column mask metadata from the
/// `/* zero-migrate:mask:kind=…,classification=… */` sentinel comments the DDL
/// emitter writes alongside every `<col>_masked` sibling column (see
/// `zeroship_data_sql::compile::build_create_table_with_fks`).
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
/// the PG arm's treatment in `zeroship-data-postgres's pg_introspect::read_live_schema` so both
/// arms surface the same "loud-but-recoverable" failure shape.
///
/// Same hand-rolled walker pattern as
/// [`parse_encryption_sentinels`] — no `regex` dep required.
fn parse_mask_sentinels(
    create_table_text: &str,
) -> std::collections::HashMap<String, zeroship_data_sql::catalog::MaskMeta> {
    use zeroship_data_sql::catalog::MaskMeta;
    let mut out = std::collections::HashMap::new();
    // Composed from the shared prefix, for the reason
    // [`parse_encryption_sentinels`] states at its own marker.
    let marker = format!("/* {}", zeroship_data_sql::mask_codec::MASK_SENTINEL_PREFIX);
    let marker = marker.as_str();
    let mut search_pos = 0usize;
    while let Some(found) = create_table_text[search_pos..].find(marker) {
        let abs_marker = search_pos + found;
        // The marker swallows the leading `/* ` so the comment body
        // starts at `zero-migrate:mask:`. We find the matching `*/` to extract
        // the full sentinel payload.
        let body_start = abs_marker + "/* ".len();
        let Some(end_rel) = create_table_text[body_start..].find("*/") else {
            // Unterminated comment: abort the walk, loudly. Same decision and
            // same reasoning as [`parse_encryption_sentinels`] states in full
            // at its own terminator search - the `*/` scan runs to end-of-text,
            // so a `None` here forces a `None` for every later marker, and
            // skipping ahead instead of breaking would recover nothing while
            // warning once per marker. The two walkers deliberately agree.
            let truncated = create_table_text[body_start..].trim();
            tracing::warn!(
                sentinel = %truncated,
                "diff: unterminated mask sentinel comment in the CREATE TABLE text; \
                 abandoning the sentinel walk for this table",
            );
            break;
        };
        let body = create_table_text[body_start..body_start + end_rel].trim();
        // Reuse the canonical parser so the wire shape is centralised.
        match zeroship_data_sql::mask_codec::parse_mask_sentinel(body) {
            Ok((kind, classification)) => {
                let before = &create_table_text[..abs_marker];
                // The sentinel rides the MASKED column, which after the
                // storage flip is the field's OWN column - so the identifier
                // preceding the comment IS the logical field name and there is
                // no suffix to strip.
                //
                // The strip that used to be here had no `else`: an identifier
                // that did not end `_masked` was discarded in silence. After
                // the flip that arm would have matched EVERY sentinel, so this
                // function would have reported every masked column as
                // unmasked, with no warning - unlike the malformed-sentinel
                // arm below, which is loud.
                match recover_preceding_quoted_ident(before) {
                    Some(column) => {
                        out.insert(
                            column.clone(),
                            MaskMeta {
                                kind,
                                classification,
                                sibling_column: zeroship_data_sql::compile::raw_column_name(
                                    &column,
                                ),
                            },
                        );
                    }
                    None => {
                        tracing::warn!(
                            sentinel = %body,
                            "diff: mask sentinel with no recoverable column name \
                             in the CREATE TABLE text; ignoring",
                        );
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    sentinel = %body,
                    error = %e,
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
// `Backup` capability (VACUUM INTO snapshot + atomic file-swap restore).
// ---------------------------------------------------------------------------
//
// Two methods on `impl Backup for SqliteBackend`:
//
//   * `snapshot(app_id, dest_uri, opts)`:
//       1. Hold the per-app snapshot/restore advisory lock through the
//          in-process `LockManager` so another backup operation cannot replace
//          the database during the copy.
//       2. Parse `dest_uri` - `file://` only (S3/HTTPS deferred,
//          mirrors the PG arm). Bare paths accepted.
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
//       1. Hold the per-app snapshot/restore lock.
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
// There was a third, `pitr_replay`, which refused unconditionally with
// `Configuration { code: "pitr_pg_only" }`. It went on 2026-09-07 with the
// trait method it implemented, because the PG arm it deferred to could not
// replay either - see `zeroship_data_orm::storage::Backup`'s rustdoc for why
// PITR is an operator capability with a database-server contract rather than a
// data-store method.
//
// The gate below is `test-helpers` ALONE. It is the IMPL that is gated, not the
// trait: `zeroship_data_orm::storage::Backup` ungated itself on 2026-09-04, on
// the grounds that a contract whose shape depends on a feature is not a
// contract. What stays out of a release build is the body - a `pg_dump` /
// `pg_restore` shell-out on one arm and a destructive file swap on the other.
// This comment claimed the trait was gated until 2026-09-07 and cited
// `backend/mod.rs`, a path that no longer holds it.

#[cfg(feature = "test-helpers")]
impl zeroship_data_orm::storage::Backup for SqliteBackend {
    async fn snapshot(
        &self,
        app_id: &str,
        dest_uri: &str,
        opts: zeroship_data_orm::capability::SnapshotOpts,
    ) -> Result<zeroship_data_orm::capability::SnapshotHandle, DbError> {
        backup_sqlite::snapshot_impl(self, app_id, dest_uri, opts).await
    }

    async fn restore(
        &self,
        app_id: &str,
        snapshot: &zeroship_data_orm::capability::SnapshotHandle,
    ) -> Result<(), DbError> {
        backup_sqlite::restore_impl(self, app_id, snapshot).await
    }
}

/// Inner module so the SQLite-side `Backup` helpers stay grouped and
/// the surrounding file keeps its "thin trait facade + per-capability
/// impl block" shape — matches the PG arm's `backup_pg` inner module
/// in `backend/postgres.rs`.
///
/// `pub(super)` so the trait methods above can call in; the helpers
/// stay private to this file.
///
/// Gated to match the `impl Backup` block above - see the note there for
/// why `test-helpers` alone is the only gate that builds.
#[cfg(feature = "test-helpers")]
mod backup_sqlite {
    use std::io::Read;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::SqliteBackend;
    use zeroship_data_orm::capability::{
        BusyPolicy, LockScope, SNAPSHOT_RESTORE_LOCK_TAG, SnapshotHandle, SnapshotOpts,
    };
    use zeroship_data_orm::error::DbError;

    // The lock tag is IMPORTED, not redeclared. This module carried its own
    // `const SNAPSHOT_RESTORE_LOCK_TAG: &str = "snapshot_restore"` until
    // 2026-09-02, whose doc comment said "It matches the PostgreSQL arm's
    // shared tag" - an agreement between two string literals, held by hand and
    // checked by nothing. `crate::backend::sqlite::backend` already exports the one both arms
    // mean, and this module already imported five of its neighbours on the
    // line above.

    /// Parse a `file:///abs/path` URI into the underlying filesystem
    /// path. Mirrors the PG arm's `parse_dest_path` shape so the SDK
    /// error codes stay consistent across backends.
    ///
    /// SQLite supports only the `file://` scheme. Bare paths
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

    /// Acquire the per-app snapshot/restore advisory lock through the
    /// in-process registry. Returns the `(key1, key2)` pair so the
    /// caller can release it symmetrically. On contention emits the
    /// typed `migration_in_progress` Coded error (mirrors the PG arm).
    async fn acquire_snapshot_restore_lock(
        backend: &SqliteBackend,
        app_id: &str,
        op: &'static str,
    ) -> Result<(String, String), DbError> {
        let scope = LockScope::GlobalApp {
            app_id: app_id.to_string(),
            name: SNAPSHOT_RESTORE_LOCK_TAG.to_string(),
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
            if backend.lock_registry.try_acquire((k1.clone(), k2.clone())) {
                return Ok((k1, k2));
            }
        }
        Err(DbError::Coded {
            code: "migration_in_progress".to_string(),
            message: format!(
                "{op}: another snapshot / restore is in progress for app {app_id:?} \
                 (in-process backup lock held; 5-attempt bounded retry exhausted)"
            ),
            hint: Some(
                "retry the operation once the in-flight snapshot / restore completes".to_string(),
            ),
        })
    }

    /// Drop the lock acquired by [`acquire_snapshot_restore_lock`].
    /// Infallible at the registry layer — unheld slots emit a
    /// `tracing::warn` no-op. Matches the contract of every other
    /// `release_advisory_lock` site.
    fn release_snapshot_restore_lock(backend: &SqliteBackend, k1: String, k2: String) {
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
        // 1. Hold the per-app snapshot/restore lock for the whole snapshot.
        let (k1, k2) = acquire_snapshot_restore_lock(backend, app_id, "snapshot").await?;

        // 2. Parse + prepare destination.
        let dest_path = match parse_dest_path(dest_uri) {
            Ok(p) => p,
            Err(e) => {
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(e);
            }
        };
        if let Some(parent) = dest_path.parent() {
            if !parent.as_os_str().is_empty() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    release_snapshot_restore_lock(backend, k1, k2);
                    return Err(DbError::Internal {
                        message: format!("snapshot: create parent dir {parent:?} failed: {e}"),
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
            release_snapshot_restore_lock(backend, k1, k2);
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
        //    the attach_app_file path attached the per-app file).
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
                    release_snapshot_restore_lock(backend, k1, k2);
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
            release_snapshot_restore_lock(backend, k1, k2);
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
                release_snapshot_restore_lock(backend, k1, k2);
                let _ = std::fs::remove_file(&dest_path);
                return Err(DbError::Internal {
                    message: format!("snapshot: SHA-256 of {dest_path_str:?} failed: {e}"),
                });
            }
        };

        // 5. Release the lock now that the snapshot is committed to
        //    disk. From here on another backup operation can proceed;
        //    the SnapshotHandle's content_hash pins integrity for the
        //    eventual restore.
        release_snapshot_restore_lock(backend, k1, k2);

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
        // 1. Hold the per-app snapshot/restore lock for the whole restore so
        //    another backup operation cannot race the DETACH/rename/ATTACH sequence.
        let (k1, k2) = acquire_snapshot_restore_lock(backend, app_id, "restore").await?;

        // 2. Resolve the snapshot URI to an on-disk path. SQLite
        //    supports file:// only.
        let src_path = match parse_dest_path(&snapshot.uri) {
            Ok(p) => p,
            Err(e) => {
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(e);
            }
        };

        // 3. Re-hash and verify integrity BEFORE touching the live
        //    DB. A mismatch means the snapshot was tampered with or
        //    truncated; refuse before any DETACH/rename.
        match sha256_file(&src_path) {
            Ok(observed) if observed == snapshot.content_hash => { /* ok */ }
            Ok(_) => {
                release_snapshot_restore_lock(backend, k1, k2);
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
                release_snapshot_restore_lock(backend, k1, k2);
                return Err(DbError::Internal {
                    message: format!("restore: SHA-256 of {src_path:?} failed: {e}"),
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
        let temp_path = backend
            .db_dir
            .join(format!("zs-{app_id}.sqlite.restore-tmp"));
        // Best-effort cleanup of a stale tmp from a prior crashed run.
        let _ = std::fs::remove_file(&temp_path);
        if let Err(e) = std::fs::copy(&src_path, &temp_path) {
            release_snapshot_restore_lock(backend, k1, k2);
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
            release_snapshot_restore_lock(backend, k1, k2);
            // Best-effort: leave the temp file in place so the
            // operator can inspect it; do NOT delete on error.
            return Err(e);
        }

        // 6. Restore complete. Release the lock.
        release_snapshot_restore_lock(backend, k1, k2);
        Ok(())
    }

    // There is no `pitr_replay` helper because there is no longer a
    // `pitr_replay`. It was deleted on 2026-09-07 together with the trait
    // method; neither backend could replay, and the reason is recorded once, in
    // `zeroship_data_orm::storage::Backup`'s rustdoc, rather than in a comment
    // per arm.
    //
    // Two earlier homes for that sentence are worth not repeating: an empty
    // `fn _pitr_marker(PitrTarget) {}` under `#[allow(dead_code)]` (deleted
    // 2026-09-04), and the refusal arm itself. A no-op function is a bad home
    // for a sentence about why code is ABSENT, because it compiles and so reads
    // as a mechanism; a refusal arm is a worse one, because it reads as a
    // deliberate per-vendor divergence when the capability was absent on both.
}

#[cfg(test)]
mod tests {
    //! Compile-time trait-shape assertions, mirroring the PR-0 set
    //! at `zeroship_data_orm::backend`'s conformance tests (which target `PostgresBackend`).
    //! These pin the SQLite-side surface so any future drift in the
    //! capability-trait composition trips compilation here rather
    //! than at a distant orchestrator / context call site.
    //!
    //! Body intentionally empty (`fn assert_impl<T: Trait>() {}`) —
    //! the bound itself is the assertion.

    use super::*;
    use crate::fixtures::DatabaseFixture;
    use zeroship_data_orm::storage::LockManager;
    // A plain `use` is private, so the module-level import does not arrive via
    // `use super::*`. UNGATED since 2026-09-04 with the trait itself.
    use zeroship_data_orm::protection::Catalog;

    #[compio::test]
    async fn memory_and_empty_database_paths_are_rejected() {
        for path in ["", ":memory:", "file:memory?mode=memory", "db?mode=memory"] {
            let error = SqliteBackend::open(
                path,
                std::sync::Arc::new(NullChangeSink),
                zeroship_data_orm::encryption::LocalKeySource::env_var(),
            )
            .await
            .unwrap_err();
            assert!(
                matches!(
                    error,
                    DbError::Configuration {
                        code: "sqlite_file_required",
                        ..
                    }
                ),
                "{error:?}",
            );
        }
    }

    #[test]
    fn spatial_near_base_query_reads_masked_sibling_when_schema_cached() {
        let schema = zeroship_data_sql::value!({
            "ssn": {
                "type": "string",
                "mask": { "kind": "last4", "classification": "spi" }
            },
            "location": { "type": "geoPoint" }
        });
        let bq = search::build_spatial_near_base_query(
            &zeroship_data_sql::SchemaName::new("app1").expect("fixture schema name"),
            "places",
            &zeroship_data_sql::value!({}),
            &schema,
        )
        .expect("spatial base query");
        assert!(
            !bq.sql.starts_with("SELECT *"),
            "spatial base query must not use SELECT * when masked columns exist: {}",
            bq.sql,
        );
        // A masked column reads its OWN column (the mask); the raw column must
        // not appear — see the twin assertion in `backend::sqlite::vector`.
        assert!(
            bq.sql.contains("\"ssn\""),
            "spatial base query must project the masked column: {}",
            bq.sql,
        );
        assert!(
            !bq.sql
                .contains(&zeroship_data_sql::compile::raw_column_name("ssn")),
            "spatial base query must never name the raw column: {}",
            bq.sql,
        );
    }

    /// `Backend` composition marker now lands on
    /// `SqliteBackend`. Pinning the bound here means a future change
    /// that detaches one of the five sub-trait impls (or that
    /// regresses the super-bound relaxation back to
    /// `Client = compio_postgres::Client`) fails compilation in this
    /// module rather than at a distant orchestrator call site.
    // `assert_sqlite_backend_impls_backend` is NOT here: `Backend` is the
    // adapter's own `pub(crate)` marker, so the impl and the assertion pinning
    // it both live in `zeroship-data-v8/src/backend/mod.rs`. The sub-trait
    // assertions below stay, because their traits are data-core's and visible.
    fn assert_sqlite_backend_impls_backend() {}

    fn assert_sqlite_backend_impls_sql_executor() {
        fn assert_impl<T: DatabaseFixture>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_lock_manager() {
        fn assert_impl<T: LockManager>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_impls_namespace_manager() {}

    // Follows `Catalog`'s own gate in data-core, which is now NONE:
    // the trait ships, because the protection floor on the write path reads
    // this catalog. The assertion is ungated with it - a witness that only
    // compiles under `test-helpers` says nothing about the build that ships.
    fn assert_sqlite_backend_impls_schema_introspect() {
        fn assert_impl<T: Catalog>() {}
        assert_impl::<SqliteBackend>();
    }

    /// `VectorIndex` capability - pin the SQLite-arm impl
    /// wire so the pure-Rust flat-scan path's trait composition
    /// regresses at compile time if the impl block is detached or
    /// the method shape drifts from the trait surface.
    fn assert_sqlite_backend_impls_vector_index() {
        fn assert_impl<T: crate::search::Search>() {}
        assert_impl::<SqliteBackend>();
    }

    /// `SpatialIndex` capability - pin the SQLite-arm impl
    /// wire so the haversine flat-scan path's trait composition
    /// regresses at compile time if the impl block is detached.
    fn assert_sqlite_backend_impls_spatial_index() {
        fn assert_impl<T: crate::search::Search>() {}
        assert_impl::<SqliteBackend>();
    }

    fn assert_sqlite_backend_is_static() {
        fn assert_static<T: 'static>() {}
        assert_static::<SqliteBackend>();
    }

    /// Pin the `DatabaseFixture::Client` associated type to the
    /// session-handle shape. A regression that swaps the type (e.g.
    /// accidentally re-pointing it to `rusqlite::Connection` rather
    /// than the actor-handle wrapper) trips here.
    fn assert_sqlite_client_pinned_to_session_handle() {
        fn assert_impl<T: DatabaseFixture<Client = SqliteSessionHandle>>() {}
        assert_impl::<SqliteBackend>();
    }

    // -----------------------------------------------------------------
    // Sentinel-walker event capture
    // -----------------------------------------------------------------

    /// One captured `tracing` event: its level plus its rendered fields.
    /// The format-string body arrives under the synthetic `"message"` key
    /// the macros assign, so it needs no separate slot.
    type CapturedEvent = (tracing::Level, std::collections::HashMap<String, String>);

    /// Collects every event emitted under it into a shared buffer.
    struct CaptureLayer {
        events: std::sync::Arc<std::sync::Mutex<Vec<CapturedEvent>>>,
    }

    #[derive(Default)]
    struct FieldVisitor {
        fields: std::collections::HashMap<String, String>,
    }

    impl tracing::field::Visit for FieldVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_string(), format!("{value:?}"));
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            self.fields
                .insert(field.name().to_string(), value.to_string());
        }
    }

    impl<S> tracing_subscriber::layer::Layer<S> for CaptureLayer
    where
        S: tracing::Subscriber + for<'lookup> tracing_subscriber::registry::LookupSpan<'lookup>,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut visitor = FieldVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .expect("capture buffer mutex poisoned")
                .push((*event.metadata().level(), visitor.fields));
        }
    }

    /// Run `f` under a capturing subscriber; return its result plus every
    /// event it emitted.
    ///
    /// This exists because the sentinel walkers signal a REFUSAL by
    /// `tracing::warn!` and nothing else - the returned map is simply missing
    /// an entry, which is byte-identical to a DDL that carried no sentinel at
    /// all. Without capture, "refused loudly" and "dropped in silence" are the
    /// same observation, which is precisely the defect the tests below pin.
    ///
    /// `with_default` installs the subscriber on the CURRENT THREAD only and
    /// libtest gives every test its own thread, so a parallel run cannot mix
    /// two tests' events.
    fn capture_events<R>(f: impl FnOnce() -> R) -> (R, Vec<CapturedEvent>) {
        use tracing_subscriber::layer::SubscriberExt;

        let events = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::registry().with(CaptureLayer {
            events: std::sync::Arc::clone(&events),
        });
        let result = tracing::subscriber::with_default(subscriber, f);
        let captured = events
            .lock()
            .expect("capture buffer mutex poisoned")
            .clone();
        (result, captured)
    }

    /// The single WARN event `f` emitted, or a failure naming what it did
    /// emit instead.
    fn sole_warning<R>(f: impl FnOnce() -> R) -> (R, std::collections::HashMap<String, String>) {
        let (result, events) = capture_events(f);
        let warnings: Vec<_> = events
            .iter()
            .filter(|(level, _)| *level == tracing::Level::WARN)
            .collect();
        assert_eq!(
            warnings.len(),
            1,
            "expected exactly one WARN; captured {events:?}"
        );
        (result, warnings[0].1.clone())
    }

    /// Assert `ddl` yields no encryption metadata AND that the walker said so
    /// out loud, carrying the codec's typed error verbatim.
    fn assert_enc_sentinel_refused_loudly(ddl: &str, expected_error_fragment: &str) {
        let (got, fields) = sole_warning(|| parse_encryption_sentinels(ddl));
        assert!(
            got.is_empty(),
            "a refused sentinel must stamp no column: {got:?}"
        );
        let error = fields
            .get("error")
            .expect("the warning must carry the codec's typed error");
        assert!(
            error.contains("enc_sentinel_malformed"),
            "the codec's discriminator must survive into the log line: {error:?}"
        );
        assert!(
            error.contains(expected_error_fragment),
            "expected {expected_error_fragment:?} in {error:?}"
        );
        assert!(
            fields.contains_key("sentinel"),
            "the warning must name the offending sentinel: {fields:?}"
        );
    }

    // -----------------------------------------------------------------
    // Encryption sentinel parser unit tests
    // -----------------------------------------------------------------

    /// Round-trip a single encrypted column: emitter shape → parser
    /// extracts `(mode, key_id, wraps)` correctly.
    #[test]
    fn parse_encryption_sentinel_single_column() {
        let ddl = "CREATE TABLE \"app\".\"users\" (\n  \
            id SERIAL PRIMARY KEY,\n  \
            \"ssn\" BYTEA /* zero-migrate:enc:default:string */  NOT NULL,\n  \
            \"name\" TEXT \n)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("ssn").expect("ssn must be parsed");
        assert_eq!(m.key_id, "default");
        assert!(matches!(
            m.wraps,
            zeroship_data_sql::catalog::WrappedType::String
        ));
        assert!(
            !got.contains_key("name"),
            "non-encrypted col must be absent"
        );
        assert!(!got.contains_key("id"));
    }

    /// Deterministic mode + non-string wraps + custom key id.
    #[test]
    fn parse_encryption_sentinel_number_custom_key() {
        let ddl = "CREATE TABLE \"app\".\"events\" (\n  \
            \"salary\" BYTEA /* zero-migrate:enc:payroll_v2:number */ NOT NULL\n)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("salary").expect("salary must be parsed");
        assert_eq!(m.key_id, "payroll_v2");
        assert!(matches!(
            m.wraps,
            zeroship_data_sql::catalog::WrappedType::Number
        ));
    }

    /// US spelling `randomized` round-trips as canonical Randomised
    /// (the DDL emitter normalises but a hand-edited DDL could carry
    /// the US form).
    #[test]
    fn parse_encryption_sentinel_accepts_us_spelling() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:default:bytes */)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("a").expect("a must be parsed");
        assert!(matches!(
            m.wraps,
            zeroship_data_sql::catalog::WrappedType::Bytes
        ));
    }

    /// Multiple encrypted columns in one CREATE TABLE — each attaches
    /// to its own column name.
    #[test]
    fn parse_encryption_sentinel_multiple_columns() {
        let ddl = "CREATE TABLE \"app\".\"u\" (\n  \
            \"ssn\" BYTEA /* zero-migrate:enc:default:string */,\n  \
            \"tin\" BYTEA /* zero-migrate:enc:tax:string */\n)";
        let got = parse_encryption_sentinels(ddl);
        assert_eq!(got.len(), 2);

        assert_eq!(got["tin"].key_id, "tax");
    }

    /// Malformed sentinel — wrong number of parts → refused, and the refusal
    /// is AUDIBLE. A column whose sentinel does not parse reads back
    /// unencrypted, so the log line is the only difference between "the
    /// metadata was rejected" and "there was never any metadata".
    #[test]
    fn parse_encryption_sentinel_rejects_malformed() {
        assert_enc_sentinel_refused_loudly(
            "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:only_one_part */)",
            "expected zero-migrate:enc:",
        );
    }

    /// Removed mode-bearing sentinels are refused.
    #[test]
    fn parse_encryption_sentinel_rejects_removed_mode() {
        assert_enc_sentinel_refused_loudly(
            "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:hashed:default:string */)",
            "expected zero-migrate:enc:",
        );
    }

    /// Unknown wraps → refused loudly. This arm had no test at all before the
    /// walker was collapsed onto the codec.
    #[test]
    fn parse_encryption_sentinel_rejects_unknown_wraps() {
        assert_enc_sentinel_refused_loudly(
            "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:default:blob */)",
            "unknown wraps",
        );
    }

    /// Empty keyId → refused loudly. There is no key to look up, so this must
    /// stay a refusal even though the codec dropped the alphabet check.
    ///
    /// The body must preserve the empty key slot (`:<wraps>`) to reach
    /// this arm; drop one and the codec refuses on ARITY first, which is a
    /// different arm and a different message.
    #[test]
    fn parse_encryption_sentinel_rejects_empty_key_id() {
        assert_enc_sentinel_refused_loudly(
            "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc::string */)",
            "empty keyId",
        );
    }

    /// A key id outside the SDK's `[A-Za-z0-9_]` alphabet is ACCEPTED.
    ///
    /// The hand-rolled walker this replaced enforced that alphabet itself and
    /// dropped anything else in silence. That check was a THIRD opinion on the
    /// wire shape: `zeroship_data_sql::mask_codec::parse_encryption_sentinel`
    /// requires only a non-empty keyId, and `t.encrypted()` already fences the
    /// alphabet at author time (`sdks/db/src/types.ts`, `/^[A-Za-z0-9_]+$/`).
    /// The decided direction is one authority for the wire, enforcement at the
    /// authoring edge — so a hand-edited DDL, or a future rotation scheme
    /// spelling ids `payroll-2026-09`, now round-trips instead of quietly
    /// reading the column back as plaintext.
    #[test]
    fn parse_encryption_sentinel_accepts_a_key_id_outside_the_sdk_alphabet() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:payroll-2026-09:string */)";
        let (got, events) = capture_events(|| parse_encryption_sentinels(ddl));
        assert_eq!(
            got.get("a").map(|m| m.key_id.as_str()),
            Some("payroll-2026-09"),
            "the codec accepts any non-empty keyId: {got:?}"
        );
        assert!(
            events.is_empty(),
            "an accepted sentinel must be silent: {events:?}"
        );
    }

    /// A well-formed sentinel with no recoverable column name in front of it
    /// is skipped — loudly. This is the walker's own failure, not the codec's,
    /// so the event carries no `error` field.
    #[test]
    fn parse_encryption_sentinel_warns_when_no_column_precedes_it() {
        let ddl = "CREATE TABLE t (\n  /* zero-migrate:enc:default:string */\n)";
        let (got, fields) = sole_warning(|| parse_encryption_sentinels(ddl));
        assert!(
            got.is_empty(),
            "a sentinel with no column must stamp nothing: {got:?}"
        );
        assert!(
            fields.contains_key("sentinel"),
            "the warning must name the orphaned sentinel: {fields:?}"
        );
    }

    /// Every `(mode, wraps)` the emitter can produce round-trips through the
    /// walker unchanged, and silently.
    ///
    /// The input is BUILT by `zeroship_data_sql::mask_codec::build_encryption_sentinel`
    /// rather than hand-written, so this pins walker-against-emitter rather
    /// than walker-against-one-literal: a change to the wire shape moves both
    /// sides and this test keeps passing, which is the point of collapsing the
    /// parse onto the codec.
    #[test]
    fn parse_encryption_sentinel_round_trips_every_built_sentinel() {
        use zeroship_data_sql::catalog::{EncryptionMeta, WrappedType};

        {
            for wraps in [WrappedType::String, WrappedType::Number, WrappedType::Bytes] {
                let meta = EncryptionMeta {
                    key_id: "default".to_string(),
                    wraps,
                };
                let sentinel = zeroship_data_sql::mask_codec::build_encryption_sentinel(&meta);
                let ddl = format!("CREATE TABLE t (\"ssn\" BYTEA /* {sentinel} */ NOT NULL)");
                let (got, events) = capture_events(|| parse_encryption_sentinels(&ddl));
                let parsed = got.get("ssn").unwrap_or_else(|| {
                    panic!("built sentinel {sentinel:?} must round-trip: {got:?}")
                });

                assert_eq!(parsed.key_id, meta.key_id, "keyId drifted for {sentinel:?}");
                assert_eq!(parsed.wraps, meta.wraps, "wraps drifted for {sentinel:?}");
                assert!(
                    events.is_empty(),
                    "the success path must be silent for {sentinel:?}: {events:?}"
                );
            }
        }
    }

    /// DDL with no sentinels → empty map (no allocations beyond the
    /// HashMap itself).
    #[test]
    fn parse_encryption_sentinel_empty_when_no_marker() {
        let ddl = "CREATE TABLE t (\"a\" TEXT, \"b\" INTEGER)";
        let got = parse_encryption_sentinels(ddl);
        assert!(got.is_empty());
    }

    /// An unterminated comment ends the walk without looping - and says so.
    ///
    /// The `break` is the walk's ONLY early exit, and it was also its only
    /// SILENT one until this assertion existed: the returned map is simply
    /// missing an entry, which is byte-identical to DDL that carried no
    /// sentinel at all.
    #[test]
    fn parse_encryption_sentinel_warns_on_an_unterminated_comment() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:default:string";
        let (got, fields) = sole_warning(|| parse_encryption_sentinels(ddl));
        assert!(
            got.is_empty(),
            "an unterminated comment must stamp nothing: {got:?}"
        );
        assert!(
            fields.contains_key("sentinel"),
            "the warning must carry the truncated comment body: {fields:?}"
        );
    }

    /// The blast radius of the `break`, pinned: everything BEFORE the
    /// unterminated comment survives, and nothing after it was recoverable in
    /// the first place.
    ///
    /// `create_table_text[body_start..].find("*/")` scans to the end of the
    /// WHOLE text, not to the end of this column's DDL, so `None` means no
    /// terminator exists anywhere after `body_start`. Every later marker starts
    /// later still, so it cannot find one either. `"c"` below is the witness:
    /// it carries a perfectly well-formed sentinel BODY and is still
    /// unrecoverable, because its own comment has no terminator. Continuing the
    /// walk instead of breaking would warn once per remaining marker and return
    /// exactly this map.
    ///
    /// The control is the same DDL with both comments closed; it differs only
    /// in the two ` */` terminators and recovers all three columns silently.
    #[test]
    fn parse_encryption_sentinel_unterminated_comment_strands_nothing_recoverable() {
        let unterminated = "CREATE TABLE t (\n  \
             \"a\" BYTEA /* zero-migrate:enc:default:string */,\n  \
             \"b\" BYTEA /* zero-migrate:enc:default:string,\n  \
             \"c\" BYTEA /* zero-migrate:enc:default:number\n)";
        let (got, fields) = sole_warning(|| parse_encryption_sentinels(unterminated));
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            vec!["a"],
            "only the column ahead of the unterminated comment survives: {got:?}"
        );
        assert!(
            fields.contains_key("sentinel"),
            "the abort must name the truncated body: {fields:?}"
        );

        let control = "CREATE TABLE t (\n  \
             \"a\" BYTEA /* zero-migrate:enc:default:string */,\n  \
             \"b\" BYTEA /* zero-migrate:enc:default:string */,\n  \
             \"c\" BYTEA /* zero-migrate:enc:default:number */\n)";
        let (got, events) = capture_events(|| parse_encryption_sentinels(control));
        let mut names: Vec<_> = got.keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            vec!["a", "b", "c"],
            "closing the comments recovers every column: {got:?}"
        );
        assert!(
            events.is_empty(),
            "the well-formed control must be silent: {events:?}"
        );
    }

    /// `recover_preceding_quoted_ident` finds the most recent quoted
    /// token before the marker position.
    #[test]
    fn recover_preceding_quoted_ident_picks_last_token() {
        let text = "CREATE TABLE \"app\".\"users\" ( \"ssn\" BYTEA ";
        assert_eq!(recover_preceding_quoted_ident(text).as_deref(), Some("ssn"));
    }

    #[test]
    fn recover_preceding_quoted_ident_handles_empty() {
        assert!(recover_preceding_quoted_ident("").is_none());
        assert!(recover_preceding_quoted_ident("no quotes here").is_none());
    }

    // -----------------------------------------------------------------
    // Mask sentinel parser
    // -----------------------------------------------------------------

    /// **SQLite introspection**: a CREATE TABLE body with an inline
    /// `/* zero-migrate:mask:kind=…,classification=… */` comment attached to the MASKED
    /// column - which after the storage flip is the field's own - gets parsed
    /// back as a `MaskMeta` on that field, naming the raw column as its
    /// sibling.
    ///
    /// The DDL below is the shape `build_create_table_with_fks` emits: the raw
    /// column carries the declared type, the field's own column is bare TEXT
    /// and carries the sentinel.
    #[test]
    fn sqlite_introspection_reads_mask_sentinel_in_create_sql() {
        use zeroship_data_sql::catalog::{Classification, MaskKind};
        let raw = zeroship_data_sql::compile::raw_column_name("ssn");
        let ddl = format!(
            "CREATE TABLE \"app\".\"users\" (\n  \
             \"id\" INTEGER PRIMARY KEY,\n  \
             \"{raw}\" TEXT,\n  \
             \"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */\n)"
        );
        let got = parse_mask_sentinels(&ddl);
        let meta = got.get("ssn").expect("mask meta on the declared field");
        assert_eq!(meta.kind, MaskKind::Last4);
        assert_eq!(meta.classification, Classification::Spi);
        assert_eq!(meta.sibling_column, raw);
        assert_eq!(
            got.len(),
            1,
            "the raw column is not itself a masked field: {got:?}"
        );
    }

    /// Multiple masked columns in one table → one entry per field.
    #[test]
    fn sqlite_introspection_multiple_masked_columns() {
        use zeroship_data_sql::catalog::{Classification, MaskKind};
        let ddl = format!(
            "CREATE TABLE t (\n  \
             \"{}\" TEXT,\n  \
             \"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */,\n  \
             \"{}\" TEXT,\n  \
             \"email\" TEXT /* zero-migrate:mask:kind=email,classification=pii */\n)",
            zeroship_data_sql::compile::raw_column_name("ssn"),
            zeroship_data_sql::compile::raw_column_name("email"),
        );
        let got = parse_mask_sentinels(&ddl);
        assert_eq!(got.len(), 2);
        assert_eq!(got.get("ssn").unwrap().kind, MaskKind::Last4);
        assert_eq!(got.get("ssn").unwrap().classification, Classification::Spi);
        assert_eq!(got.get("email").unwrap().kind, MaskKind::Email);
        assert_eq!(
            got.get("email").unwrap().classification,
            Classification::Pii
        );
    }

    /// A sentinel with no recoverable column name before it is ignored, and
    /// WARNS rather than being discarded in silence.
    ///
    /// This test used to assert that a sentinel on a column NOT ending
    /// `_masked` was ignored - which, after the flip, is where every sentinel
    /// legitimately sits. Keeping it would have asserted that the introspector
    /// must drop all mask metadata.
    #[test]
    fn sqlite_introspection_ignores_a_sentinel_with_no_column() {
        let ddl = "CREATE TABLE t (\n  /* zero-migrate:mask:kind=last4,classification=spi */\n)";
        let got = parse_mask_sentinels(ddl);
        assert!(
            got.is_empty(),
            "a sentinel with no column must not stamp anything: {got:?}"
        );
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
        let ddl = "CREATE TABLE t (\n  \"ssn\" TEXT,\n  \
             \"ssn_masked\" TEXT NOT NULL /* zero-migrate:mask:kind=cosmic,classification=pii */\n)";
        let got = parse_mask_sentinels(ddl);
        assert!(
            got.is_empty(),
            "malformed sentinel must NOT stamp parent: {got:?}"
        );
    }

    /// An unterminated mask comment ends the walk without looping - and says
    /// so. The encryption sibling's twin; see
    /// [`parse_encryption_sentinel_warns_on_an_unterminated_comment`] for why
    /// the silence was the defect rather than the abort.
    #[test]
    fn sqlite_introspection_warns_on_an_unterminated_comment() {
        let ddl = "CREATE TABLE t (\"ssn_masked\" TEXT NOT NULL /* zero-migrate:mask:kind=last4,classification=spi";
        let (got, fields) = sole_warning(|| parse_mask_sentinels(ddl));
        assert!(
            got.is_empty(),
            "an unterminated comment must stamp nothing: {got:?}"
        );
        assert!(
            fields.contains_key("sentinel"),
            "the warning must carry the truncated comment body: {fields:?}"
        );
    }

    /// The mask twin of
    /// [`parse_encryption_sentinel_unterminated_comment_strands_nothing_recoverable`]:
    /// columns ahead of the unterminated comment survive, and `"phone"` -
    /// whose sentinel body is well-formed - is unrecoverable only because its
    /// own comment has no terminator either, which is forced by the `*/` search
    /// running to end-of-text.
    #[test]
    fn sqlite_introspection_unterminated_comment_strands_nothing_recoverable() {
        let unterminated = "CREATE TABLE t (\n  \
             \"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */,\n  \
             \"email\" TEXT /* zero-migrate:mask:kind=email,classification=pii,\n  \
             \"phone\" TEXT /* zero-migrate:mask:kind=last4,classification=spi\n)";
        let (got, fields) = sole_warning(|| parse_mask_sentinels(unterminated));
        assert_eq!(
            got.keys().collect::<Vec<_>>(),
            vec!["ssn"],
            "only the column ahead of the unterminated comment survives: {got:?}"
        );
        assert!(
            fields.contains_key("sentinel"),
            "the abort must name the truncated body: {fields:?}"
        );

        let control = "CREATE TABLE t (\n  \
             \"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */,\n  \
             \"email\" TEXT /* zero-migrate:mask:kind=email,classification=pii */,\n  \
             \"phone\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */\n)";
        let (got, events) = capture_events(|| parse_mask_sentinels(control));
        let mut names: Vec<_> = got.keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            vec!["email", "phone", "ssn"],
            "closing the comments recovers every column: {got:?}"
        );
        assert!(
            events.is_empty(),
            "the well-formed control must be silent: {events:?}"
        );
    }

    #[test]
    fn compile_time_trait_assertions_link() {
        // Keep the asserter functions live — same convention as the
        // PG-side `compile_time_assertions_link`.
        let _ = assert_sqlite_backend_impls_backend as fn();
        let _ = assert_sqlite_backend_impls_sql_executor as fn();
        let _ = assert_sqlite_backend_impls_lock_manager as fn();
        let _ = assert_sqlite_backend_impls_namespace_manager as fn();
        #[cfg(feature = "test-helpers")]
        let _ = assert_sqlite_backend_impls_schema_introspect as fn();
        let _ = assert_sqlite_backend_impls_vector_index as fn();
        let _ = assert_sqlite_backend_impls_spatial_index as fn();
        let _ = assert_sqlite_backend_is_static as fn();
        let _ = assert_sqlite_client_pinned_to_session_handle as fn();
    }
}

mod params;

pub mod driver;
mod executor;
mod protection;
mod search;

impl crate::backend::Backend for SqliteBackend {
    fn publishes_committed_changes(&self) -> bool {
        true
    }
}
