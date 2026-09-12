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

#[cfg(test)]
use crate::value::Value;

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
pub mod session;
// Pure-Rust haversine + `(lat, lng)` BLOB round-trip. The
// `impl SpatialIndex for SqliteBackend` block at the bottom of
// this file routes the flat-scan path through this module; the math
// (`haversine_m`) and the `point_to_blob` / `blob_to_point` helpers
// stay unit-testable in `spatial.rs`.
pub mod spatial;
// SQLite vector metric validation.

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
    /// Project encryption keys supplied by the trusted host.
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
        params: &[crate::value::Value],
    ) -> Result<Vec<crate::value::Value>, DbError> {
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
        key_source: zeroship_data_orm::encryption::ProjectKeySource,
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
        key_source: zeroship_data_orm::encryption::ProjectKeySource,
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
        key_source: zeroship_data_orm::encryption::ProjectKeySource,
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

    /// Attach the host's change sink and project keys to the opened backend.
    fn finish_open(
        opened: OpenedBackend,
        sink: Arc<dyn ChangeSink>,
        key_source: zeroship_data_orm::encryption::ProjectKeySource,
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
        // Project keys are supplied by the host, independently of the database.
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
    /// Return an unreserved session handle for autocommit commands.
    /// Each command receives its own reservation on the actor's autocommit connection.
    pub fn autocommit_client(&self) -> SqliteSessionHandle {
        SqliteSessionHandle::new(self.session.clone())
    }

    /// **Test-only**: a transaction-lane handle the actor never bound. See
    /// [`session::SqliteSession::unregistered_transaction_handle_for_tests`].
    #[cfg(test)]
    pub fn unregistered_transaction_client_for_tests(&self) -> SqliteSessionHandle {
        self.session.unregistered_transaction_handle_for_tests()
    }

    /// **Test-only**: stall the next command *this backend's* session runs. See
    /// [`session::SqliteSession::arm_next_command_gate_for_tests`]; the gate is
    /// per-session, so it cannot be tripped by another backend's traffic.
    #[cfg(test)]
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
    #[cfg(test)]
    pub fn spent_autocommit_reservation_for_tests(
        &self,
    ) -> std::sync::Arc<crate::backend::sqlite::reservation::Reservation> {
        self.session.autocommit_reservation_for_tests()
    }

    /// **Test-only**: run one `Exec` under a caller-held reservation.
    #[cfg(test)]
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
    #[cfg(test)]
    pub async fn settle_transaction_for_tests(
        &self,
        client: &SqliteSessionHandle,
        intent: session::TerminalIntent,
    ) -> Result<crate::backend::sqlite::reservation::TerminalOutcome, DbError> {
        client.settle(intent).await
    }
}

#[cfg(test)]
impl crate::tests::fixtures::DatabaseFixture for SqliteBackend {
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
    // SQLite locks share this backend instance’s registry; they do not use the SQL
    // connection or coordinate other backend instances.

    async fn acquire_advisory_lock(
        &self,
        _client: &Self::Client,
        key1: &str,
        key2: &str,
    ) -> Result<(), DbError> {
        // This primitive waits indefinitely. Use the typed lock API for bounded retries.
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
    /// Attach the app’s database file under its schema alias, caching successful attaches.
    /// Schema changes are owned by the migration engine.
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
// Vector search uses sqlite-vec's scalar distance functions on the base BLOB
// column. The extension is linked statically and registered by the session
// host. SQLite development requires no extra tables or triggers; the ORM
// applies filters before ranking on the captured transaction or autocommit lane.

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

/// Recover encrypted-column metadata from stored DDL comments.
/// The scanner attaches each sentinel to the preceding quoted column identifier.
/// Malformed or unattachable sentinels are logged and skipped.
fn parse_encryption_sentinels(
    create_table_text: &str,
) -> std::collections::HashMap<String, crate::sql::catalog::EncryptionMeta> {
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
    let marker = format!("/* {}", crate::sql::mask_codec::ENC_SENTINEL_PREFIX);
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
        match crate::sql::mask_codec::parse_encryption_sentinel(body) {
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

/// Recover mask metadata from sentinels on visible field columns.
/// The result is keyed by logical field name. Malformed or unattachable sentinels
/// are logged and skipped, matching PostgreSQL catalog recovery.
fn parse_mask_sentinels(
    create_table_text: &str,
) -> std::collections::HashMap<String, crate::sql::catalog::MaskMeta> {
    use crate::sql::catalog::MaskMeta;
    let mut out = std::collections::HashMap::new();
    // Composed from the shared prefix, for the reason
    // [`parse_encryption_sentinels`] states at its own marker.
    let marker = format!("/* {}", crate::sql::mask_codec::MASK_SENTINEL_PREFIX);
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
        match crate::sql::mask_codec::parse_mask_sentinel(body) {
            Ok((kind, classification)) => {
                let before = &create_table_text[..abs_marker];
                // The identifier before the sentinel is the visible field column.
                match recover_preceding_quoted_ident(before) {
                    Some(column) => {
                        out.insert(
                            column.clone(),
                            MaskMeta {
                                kind,
                                classification,
                                raw_column: crate::sql::compile::raw_column_name(&column),
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
#[cfg(test)]
#[path = "snapshot_fixture.rs"]
mod snapshot_fixture;

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
    use crate::tests::fixtures::DatabaseFixture;
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
                zeroship_data_orm::encryption::ProjectKeySource::unavailable(),
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
    /// extracts the wrapped type correctly.
    #[test]
    fn parse_encryption_sentinel_single_column() {
        let ddl = "CREATE TABLE \"app\".\"users\" (\n  \
            id SERIAL PRIMARY KEY,\n  \
            \"ssn\" BYTEA /* zero-migrate:enc:string */  NOT NULL,\n  \
            \"name\" TEXT \n)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("ssn").expect("ssn must be parsed");
        assert!(matches!(m.wraps, crate::sql::catalog::WrappedType::String));
        assert!(
            !got.contains_key("name"),
            "non-encrypted col must be absent"
        );
        assert!(!got.contains_key("id"));
    }

    /// A numeric wrapped type survives introspection.
    #[test]
    fn parse_encryption_sentinel_number() {
        let ddl = "CREATE TABLE \"app\".\"events\" (\n  \
            \"salary\" BYTEA /* zero-migrate:enc:number */ NOT NULL\n)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("salary").expect("salary must be parsed");
        assert!(matches!(m.wraps, crate::sql::catalog::WrappedType::Number));
    }

    /// Byte-valued plaintext retains its wrapped type.
    #[test]
    fn parse_encryption_sentinel_bytes() {
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:bytes */)";
        let got = parse_encryption_sentinels(ddl);
        let m = got.get("a").expect("a must be parsed");
        assert!(matches!(m.wraps, crate::sql::catalog::WrappedType::Bytes));
    }

    /// Multiple encrypted columns in one CREATE TABLE — each attaches
    /// to its own column name.
    #[test]
    fn parse_encryption_sentinel_multiple_columns() {
        let ddl = "CREATE TABLE \"app\".\"u\" (\n  \
            \"ssn\" BYTEA /* zero-migrate:enc:string */,\n  \
            \"tin\" BYTEA /* zero-migrate:enc:string */\n)";
        let got = parse_encryption_sentinels(ddl);
        assert_eq!(got.len(), 2);
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

    /// Unknown wraps → refused loudly. This arm had no test at all before the
    /// walker was collapsed onto the codec.
    #[test]
    fn parse_encryption_sentinel_rejects_unknown_wraps() {
        assert_enc_sentinel_refused_loudly(
            "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:default:blob */)",
            "expected zero-migrate:enc:",
        );
    }

    /// A well-formed sentinel with no recoverable column name in front of it
    /// is skipped — loudly. This is the walker's own failure, not the codec's,
    /// so the event carries no `error` field.
    #[test]
    fn parse_encryption_sentinel_warns_when_no_column_precedes_it() {
        let ddl = "CREATE TABLE t (\n  /* zero-migrate:enc:string */\n)";
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

    /// Every wrapped type the emitter can produce round-trips through the
    /// walker unchanged, and silently.
    ///
    /// The input is BUILT by `crate::sql::mask_codec::build_encryption_sentinel`
    /// rather than hand-written, so this pins walker-against-emitter rather
    /// than walker-against-one-literal: a change to the wire shape moves both
    /// sides and this test keeps passing, which is the point of collapsing the
    /// parse onto the codec.
    #[test]
    fn parse_encryption_sentinel_round_trips_every_built_sentinel() {
        use crate::sql::catalog::{EncryptionMeta, WrappedType};

        {
            for wraps in [WrappedType::String, WrappedType::Number, WrappedType::Bytes] {
                let meta = EncryptionMeta { wraps };
                let sentinel = crate::sql::mask_codec::build_encryption_sentinel(&meta);
                let ddl = format!("CREATE TABLE t (\"ssn\" BYTEA /* {sentinel} */ NOT NULL)");
                let (got, events) = capture_events(|| parse_encryption_sentinels(&ddl));
                let parsed = got.get("ssn").unwrap_or_else(|| {
                    panic!("built sentinel {sentinel:?} must round-trip: {got:?}")
                });
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
        let ddl = "CREATE TABLE t (\"a\" BYTEA /* zero-migrate:enc:string";
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
             \"a\" BYTEA /* zero-migrate:enc:string */,\n  \
             \"b\" BYTEA /* zero-migrate:enc:string,\n  \
             \"c\" BYTEA /* zero-migrate:enc:number\n)";
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
             \"a\" BYTEA /* zero-migrate:enc:string */,\n  \
             \"b\" BYTEA /* zero-migrate:enc:string */,\n  \
             \"c\" BYTEA /* zero-migrate:enc:number */\n)";
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
        use crate::sql::catalog::{Classification, MaskKind};
        let raw = crate::sql::compile::raw_column_name("ssn");
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
        assert_eq!(meta.raw_column, raw);
        assert_eq!(
            got.len(),
            1,
            "the raw column is not itself a masked field: {got:?}"
        );
    }

    /// Multiple masked columns in one table → one entry per field.
    #[test]
    fn sqlite_introspection_multiple_masked_columns() {
        use crate::sql::catalog::{Classification, MaskKind};
        let ddl = format!(
            "CREATE TABLE t (\n  \
             \"{}\" TEXT,\n  \
             \"ssn\" TEXT /* zero-migrate:mask:kind=last4,classification=spi */,\n  \
             \"{}\" TEXT,\n  \
             \"email\" TEXT /* zero-migrate:mask:kind=email,classification=pii */\n)",
            crate::sql::compile::raw_column_name("ssn"),
            crate::sql::compile::raw_column_name("email"),
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

    /// A sentinel with no recoverable column name before it is ignored.
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
        let ddl = "CREATE TABLE t (\n  \
             \"ssn\" TEXT NOT NULL /* zero-migrate:mask:kind=cosmic,classification=pii */\n)";
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
        let ddl = "CREATE TABLE t (\"ssn\" TEXT NOT NULL /* zero-migrate:mask:kind=last4,classification=spi";
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
        #[cfg(test)]
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
    fn sql_registration(&self) -> crate::sql::registration::SqlRegistration {
        crate::sql::registration::SqlRegistration::sqlite()
    }

    fn publishes_committed_changes(&self) -> bool {
        true
    }
}
