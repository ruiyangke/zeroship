//! `SqliteSession` — single-writer actor wrapping a `rusqlite::Connection`.
//!
//! **P1 PR 2** lights this up: a `compio::runtime::spawn_blocking`
//! worker thread owns the only `rusqlite::Connection` for the backend
//! and drains a bounded [`flume`] mpsc queue of [`Command`]s. The
//! actor pattern serialises every DDL / DML / DQL through a single
//! thread by construction, which is exactly the access model SQLite
//! prefers (one writer at a time; readers are serialised behind the
//! same queue in PR 2 — design §18 Q8's "1 writer + 4 readers" split
//! lands in a later PR).
//!
//! **Bootstrap PRAGMAs** (`docs/proposals/p1-sqlite-implementation-plan.md`
//! §2.2 + design §6.2.1): on `open` the worker runs
//!
//! ```sql
//! PRAGMA journal_mode = WAL;
//! PRAGMA synchronous = NORMAL;
//! PRAGMA busy_timeout = 5000;
//! PRAGMA foreign_keys = ON;
//! ```
//!
//! `journal_mode=WAL` enables concurrent readers + a single writer;
//! `synchronous=NORMAL` is the WAL-recommended fsync cadence (durable
//! across crashes, not across power loss — the design accepts this
//! trade-off because durability is layered on top by the operator's
//! filesystem replication, §6.2.1). `busy_timeout=5000` gives SQLite a
//! 5-second internal retry budget before surfacing
//! `SQLITE_BUSY` to us. `foreign_keys=ON` enables FK enforcement
//! globally (it's a per-connection setting — SQLite ships it OFF for
//! historical reasons).
//!
//! On any PRAGMA failure the worker signals startup-failure back via a
//! one-shot reply channel and exits cleanly; `SqliteSession::open`
//! propagates the `DbError` to the caller.
//!
//! **Cancellation safety**: every queued [`Command`] carries a
//! `flume::bounded(1)` reply. Cancelling the awaiting future drops the
//! receiver — the worker still runs the SQL to completion (it's
//! synchronous from the thread's view) and the next `recv` picks up
//! the next command. WAL durability is unaffected.

use std::path::Path;
use std::rc::Rc;
use std::sync::Once;

use rusqlite::Connection;

use crate::backend::sqlite::cdc::CommitPacket;
use crate::backend::sqlite::error::from_sqlite;
use crate::error::DbError;

/// One-shot `sqlite-vec` auto-extension registration.
///
/// **P4 PR 7**: `sqlite_vec::sqlite3_vec_init` is the C extension's
/// initialiser; `rusqlite::ffi::sqlite3_auto_extension` registers a
/// callback that fires for every subsequent `sqlite3_open*` call in the
/// process. The hook IS process-global by design (it lives inside the
/// linked SQLite amalgamation's auto-extension table), so we register
/// exactly ONCE at the first `SqliteSession::open` and rely on every
/// subsequent connection (writer, future readers) inheriting the
/// extension automatically.
///
/// **Safety**: `sqlite3_auto_extension` is FFI-unsafe — the C signature
/// is `int (*)(sqlite3*, char**, const sqlite3_api_routines*)` and we
/// cast `sqlite3_vec_init` (whose signature matches the C contract per
/// the upstream `sqlite-vec` crate) through `std::mem::transmute`. The
/// cast is documented in the upstream Rust example at
/// `examples/simple-rust/demo.rs` and is the canonical integration
/// pattern.
static VEC_INIT: Once = Once::new();

#[allow(unsafe_code)]
fn register_sqlite_vec_once() {
    VEC_INIT.call_once(|| {
        // SAFETY: `sqlite3_vec_init` matches the auto-extension callback
        // signature expected by the linked SQLite amalgamation (see
        // the upstream `sqlite-vec` crate's `examples/simple-rust/demo.rs`
        // — that file is the canonical integration recipe). The
        // registration is process-global and fires for every connection
        // opened thereafter; see the module-level rustdoc on `VEC_INIT`.
        unsafe {
            rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
                *const (),
                unsafe extern "C" fn(
                    *mut rusqlite::ffi::sqlite3,
                    *mut *mut std::os::raw::c_char,
                    *const rusqlite::ffi::sqlite3_api_routines,
                ) -> std::os::raw::c_int,
            >(
                sqlite_vec::sqlite3_vec_init as *const ()
            )));
        }
    });
}

/// One row of a `Query` result. Each cell is captured as a UTF-8
/// string so the result is `Send` (rusqlite's native value types
/// borrow from the statement; we materialise to owned `Option<String>`
/// here so the reply can cross the actor / future boundary).
///
/// **Why text-only at PR 2**: the PG executor's `pool_exec` /
/// `client_exec` surface takes `&[&str]` params and ignores typed
/// returns (the SDK consumes rows via the higher-level `crud` layer
/// that runs against PG today). PR 2 mirrors the surface so the
/// SqlExecutor impl can return row counts; typed-row consumers for
/// the SQLite arm follow in PR 4's `SchemaIntrospect` PRAGMA walk.
#[allow(dead_code)]
pub type Row = Vec<Option<String>>;

/// A typed SQLite cell — preserves the underlying storage-class
/// discriminator across the actor boundary instead of collapsing every
/// value to `Option<String>`. Introduced in P4 PR 4 (then carried
/// across the PR 7 vec0 swap), this is the row-decoder shape the
/// `vector_search` / `fts_search` / `spatial_near` paths consume: each
/// emits `_distance` / `_rank` annotated JSON rows whose non-vector
/// columns benefit from typed (numeric / boolean) round-tripping so
/// the SDK doesn't see "everything is a string".
#[derive(Debug, Clone)]
pub enum TypedCell {
    /// SQLite `NULL`.
    Null,
    /// `INTEGER` storage class. `i64` covers the full 8-byte SQLite
    /// integer range.
    Integer(i64),
    /// `REAL` storage class. SQLite reals are 8-byte IEEE-754.
    Real(f64),
    /// `TEXT` storage class. Decoded to UTF-8; non-UTF-8 TEXT surfaces
    /// as a `DbError::Internal` at the worker (matches the existing
    /// `run_query` behaviour at session.rs:516-522).
    Text(String),
    /// `BLOB` storage class. Owned bytes — copied out of the
    /// rusqlite-managed buffer at decode time so the reply is `Send`.
    Blob(Vec<u8>),
}

/// A typed row + column names, returned by the `QueryTyped` command
/// variant. Consumers: [`super::SqliteBackend::vector_search`] (P4
/// PR 7 vec0 JOIN result), `fts_search` (PR 5), `spatial_near`
/// (PR 5).
///
/// Column names are carried alongside the cells so each caller can
/// build a `serde_json::Value` row map without re-issuing a `PRAGMA
/// table_info` round-trip. The names live once per result-set in
/// `columns`, not per row — the worker copies from
/// `stmt.column_names()` once before the row loop.
#[derive(Debug, Clone)]
pub struct TypedRows {
    /// Column names in result-set order. Length matches every row's
    /// cell count.
    pub columns: Vec<String>,
    /// One [`TypedCell`] per cell, row-major.
    pub rows: Vec<Vec<TypedCell>>,
}

/// Commands queued onto the [`SqliteSession`] actor.
///
/// Every variant carries a `flume::Sender<…>` reply head; the caller
/// awaits the matching receiver. Sending on a dropped reply channel
/// is a no-op — the SQL has already committed (or rolled back) by
/// then, so the only externally observable consequence is the missed
/// completion notification.
pub(crate) enum Command {
    /// Run a non-row-returning statement; reply with the SQLite
    /// `Connection::changes()` value (cast to `u64`).
    Exec {
        sql: String,
        params: Vec<String>,
        reply: flume::Sender<Result<u64, DbError>>,
    },
    /// Run a row-returning statement; reply with the materialised
    /// rows. PR 4 routes
    /// [`super::SqliteBackend::introspect_schema`] +
    /// [`super::SqliteBackend::estimate_row_count`] through this
    /// variant for the PRAGMA-walk catalog inspection.
    Query {
        sql: String,
        params: Vec<String>,
        reply: flume::Sender<Result<Vec<Row>, DbError>>,
    },
    /// Run a row-returning statement; reply with typed rows + column
    /// names. Consumers (added in P4 PR 4 / PR 5):
    /// [`super::SqliteBackend::vector_search`] (the vec0 JOIN result
    /// path; PR 7 retained the typed surface for the post-search row
    /// re-emission to JSON), `fts_search` (FTS5 + bm25 ranking),
    /// `spatial_near` (haversine distance ordering). All three need
    /// numeric / blob / NULL discrimination at the row-out boundary.
    QueryTyped {
        sql: String,
        params: Vec<String>,
        reply: flume::Sender<Result<TypedRows, DbError>>,
    },
    /// `ATTACH DATABASE 'file:{db_path}' AS '<app_id>'`. Variant lands
    /// in PR 2 (so the enum shape is final) but the
    /// `NamespaceManager` impl that emits it lives in PR 3.
    #[allow(dead_code)]
    Attach {
        app_id: String,
        db_path: String,
        reply: flume::Sender<Result<(), DbError>>,
    },
    /// `DETACH DATABASE '<app_id>'`. Symmetric to `Attach`.
    #[allow(dead_code)]
    Detach {
        app_id: String,
        reply: flume::Sender<Result<(), DbError>>,
    },
    /// Drain the queue and exit the worker thread. Sent best-effort
    /// from [`SqliteSession`]'s `Drop` impl.
    Shutdown,
}

/// Single-writer actor wrapping a `rusqlite::Connection`.
///
/// Owns the `flume::Sender<Command>` head of the actor's command
/// queue; the matching receiver lives on the spawned worker thread.
/// Dropping the session sends `Shutdown` best-effort and the worker
/// joins on the next loop iteration.
///
/// **Why `std::thread::spawn` and not `compio::runtime::spawn_blocking`**:
/// compio's `spawn_blocking` submits the closure as an `Asyncify` op
/// whose completion the runtime poller observes. The session's
/// command loop is long-lived (it lives for the backend's lifetime),
/// which holds the io_uring slot for the entire process — and on
/// shutdown the runtime might already be torn down before the worker
/// observes `Shutdown`. A plain OS thread sidesteps both concerns:
/// the worker runs independently of the compio runtime, the
/// `flume::Sender::send_async` future on the caller side awaits
/// purely on flume's atomic-park primitives (no io_uring involvement),
/// and clean shutdown is fully signalled by the `Shutdown` command.
pub struct SqliteSession {
    tx: flume::Sender<Command>,
    /// Worker `JoinHandle`. Held only so the OS thread is tracked
    /// (we never join it from the session side — cancellation is
    /// signalled via the `Shutdown` command + sender drop, and the
    /// thread exits at the next loop iteration).
    _worker: std::thread::JoinHandle<()>,
}

impl std::fmt::Debug for SqliteSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Opaque — mirrors `PostgresBackend`'s Debug. The internals
        // are an mpsc head + an opaque task handle; surfacing either
        // is operationally noisy.
        f.debug_struct("SqliteSession").finish()
    }
}

impl SqliteSession {
    /// Open a SQLite database at `db_path` and spawn the writer
    /// actor.
    ///
    /// The worker thread runs the bootstrap PRAGMA sequence
    /// synchronously before entering the receive loop; any PRAGMA
    /// failure surfaces here as a typed [`DbError`] and the worker
    /// thread exits without ever serving a `Command`.
    ///
    /// **P2 PR 2 CDC integration**: when both `app_id` and
    /// `packet_tx` are `Some`, the worker installs the
    /// `preupdate_hook`/`commit_hook`/`rollback_hook` triplet via
    /// [`crate::backend::sqlite::cdc::install`] before entering the
    /// receive loop. The returned dispatcher is held on the worker's
    /// stack frame for the lifetime of the connection so the hooks'
    /// captured state outlives every write. Sessions opened with
    /// `None` install no hooks (the control session — see
    /// `SqliteBackend::new` rustdoc).
    ///
    /// The `app_id` parameter is currently unused inside the
    /// dispatcher (per-event app_id is derived from the
    /// preupdate hook's `db_name` argument — the ATTACH alias); it is
    /// retained on the signature so a future PR can repoint it.
    pub(crate) fn open(
        db_path: &Path,
        app_id: Option<&str>,
        packet_tx: Option<flume::Sender<CommitPacket>>,
    ) -> Result<Self, DbError> {
        // Bound the queue at 64 in-flight commands. The single-writer
        // actor means there is no parallelism downstream; a bigger
        // queue just delays backpressure. 64 is the same default
        // `crates/sandbox/src/db.rs` uses for its phase-1 worker queue.
        let (tx, rx) = flume::bounded::<Command>(64);

        // One-shot startup channel so the spawning task surfaces
        // PRAGMA / open failures synchronously to the caller without
        // having to drain the main command queue first.
        let (startup_tx, startup_rx) = flume::bounded::<Result<(), DbError>>(1);

        // The `db_path` is `&Path` — we need an owned `PathBuf` to
        // move into the worker closure (the closure is `'static`).
        let db_path = db_path.to_path_buf();
        // Owned copies for the worker closure. The dispatcher install
        // step uses these AFTER the PRAGMA bootstrap; for the no-CDC
        // case both are `None` and the install step short-circuits.
        let app_id_owned: Option<String> = app_id.map(|s| s.to_string());
        let packet_tx_owned = packet_tx;

        let worker = std::thread::Builder::new()
            .name("sqlite-session".to_string())
            .spawn(move || {
            // 0. Register `sqlite-vec` as a SQLite auto-extension. This
            //    fires once per process; every connection opened
            //    thereafter (including the one a few lines below) loads
            //    the `vec0` virtual table module + the `vec_*` scalar
            //    functions automatically. See `VEC_INIT` rustdoc.
            register_sqlite_vec_once();

            // 1. Open the connection. Any failure here is reported
            //    via the startup channel and the worker exits.
            let conn = match Connection::open(&db_path) {
                Ok(c) => c,
                Err(e) => {
                    let _ = startup_tx.send(Err(from_sqlite(e)));
                    return;
                }
            };

            // 2. Bootstrap PRAGMAs (design §6.2.1). Each statement
            //    runs via `execute_batch` — `PRAGMA journal_mode=WAL`
            //    returns a row ("wal"); `execute_batch` ignores
            //    returned rows, which is the cleanest way to run
            //    several PRAGMAs in one call without paying for
            //    per-statement prepare round-trips.
            //
            //    Order matters: `journal_mode=WAL` MUST come before
            //    `synchronous=NORMAL` because the latter's safety
            //    semantics differ across rollback vs WAL mode (in
            //    WAL, NORMAL is safe; in rollback mode, FULL is the
            //    only crash-safe choice). Setting journal_mode first
            //    ensures we are in WAL when synchronous is evaluated.
            const BOOT_PRAGMAS: &str = "\
                PRAGMA journal_mode = WAL; \
                PRAGMA synchronous = NORMAL; \
                PRAGMA busy_timeout = 5000; \
                PRAGMA foreign_keys = ON;";
            if let Err(e) = conn.execute_batch(BOOT_PRAGMAS) {
                let _ = startup_tx.send(Err(from_sqlite(e)));
                return;
            }

            // 2b. P2 PR 2 — install the CDC hook triplet on this
            //     connection. The dispatcher is bound to a local
            //     `_dispatcher` so its owned `Arc<Mutex<…>>` clones
            //     outlive `conn` (rusqlite stores the boxed hook
            //     closures inside `InnerConnection` and frees them at
            //     `Connection::drop`; the dispatcher's only role
            //     post-install is to keep the captured `Arc`s alive,
            //     which is automatic via Rust ownership). Sessions
            //     opened without a packet_tx skip this step entirely
            //     (the control session — see `SqliteBackend::new`).
            let _dispatcher = if let Some(tx) = packet_tx_owned {
                match crate::backend::sqlite::cdc::install(&conn, app_id_owned, tx) {
                    Ok(d) => Some(d),
                    Err(e) => {
                        let _ = startup_tx.send(Err(e));
                        return;
                    }
                }
            } else {
                None
            };

            // 3. Bootstrap succeeded — release the caller. From here
            //    on, errors flow through individual `Command::reply`
            //    channels and the worker keeps running.
            let _ = startup_tx.send(Ok(()));

            // 4. Command loop. `rx.recv()` blocks the worker thread
            //    (we're on a dedicated OS thread — blocking is the
            //    intended steady state); a disconnect from the
            //    sender side surfaces as `Err(_)` and ends the loop
            //    just like `Shutdown`.
            while let Ok(cmd) = rx.recv() {
                match cmd {
                    Command::Exec { sql, params, reply } => {
                        let result = run_exec(&conn, &sql, &params);
                        let _ = reply.send(result);
                    }
                    Command::Query { sql, params, reply } => {
                        let result = run_query(&conn, &sql, &params);
                        let _ = reply.send(result);
                    }
                    Command::QueryTyped { sql, params, reply } => {
                        let result = run_query_typed(&conn, &sql, &params);
                        let _ = reply.send(result);
                    }
                    Command::Attach { app_id, db_path, reply } => {
                        let result = run_attach(&conn, &app_id, &db_path);
                        let _ = reply.send(result);
                    }
                    Command::Detach { app_id, reply } => {
                        let result = run_detach(&conn, &app_id);
                        let _ = reply.send(result);
                    }
                    Command::Shutdown => break,
                }
            }
            // `conn` drops here, closing the SQLite connection
            // cleanly. WAL checkpointing happens on close.
            })
            .map_err(|e| {
                DbError::internal(format!(
                    "SqliteSession::open: failed to spawn worker thread: {e}"
                ))
            })?;

        // Wait synchronously for the startup signal. The worker
        // thread runs independently of any compio runtime — flume's
        // `recv()` parks on an `std::thread::park`, so blocking
        // briefly here does not stall the runtime. This constructor
        // is called once per backend at boot; the worker either
        // reports success or failure within four PRAGMA calls, so
        // the brief block is bounded.
        //
        // Returning `Err` here drops the `worker` `JoinHandle`. The
        // worker has already exited on a PRAGMA failure (we sent
        // back the error then `return`d from the closure), so no
        // dangling thread.
        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self { tx, _worker: worker }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(DbError::internal(
                "SqliteSession::open: worker exited before sending startup signal",
            )),
        }
    }

    /// Send an `Exec` command and await the reply.
    pub(crate) async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<u64, DbError>>(1);
        let cmd = Command::Exec {
            sql: sql.to_string(),
            params: params.iter().map(|s| s.to_string()).collect(),
            reply: reply_tx,
        };
        self.send(cmd).await?;
        recv_reply(reply_rx).await?
    }

    /// Send a `Query` command and await the materialised row slice.
    ///
    /// **PR 4** consumer: [`super::SqliteBackend::introspect_schema`]
    /// + [`super::SqliteBackend::estimate_row_count`] route through
    /// this method to read PRAGMA / COUNT(*) results. Each call is one
    /// round-trip through the actor's mpsc queue + one
    /// `rusqlite::Statement` lifecycle on the worker thread.
    pub(crate) async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<Vec<Row>, DbError>>(1);
        let cmd = Command::Query {
            sql: sql.to_string(),
            params: params.iter().map(|s| s.to_string()).collect(),
            reply: reply_tx,
        };
        self.send(cmd).await?;
        recv_reply(reply_rx).await?
    }

    /// Send a `QueryTyped` command and await typed rows + column names.
    ///
    /// Consumers: [`super::SqliteBackend::vector_search`] (P4 PR 7
    /// vec0 JOIN row decode), `fts_search` (PR 5 bm25 + row re-emit),
    /// `spatial_near` (PR 5 haversine distance + row re-emit). The
    /// [`TypedCell`] discriminant lets each caller branch on
    /// numeric / blob / NULL at the JSON encoder layer.
    pub(crate) async fn query_typed(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<TypedRows, DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<TypedRows, DbError>>(1);
        let cmd = Command::QueryTyped {
            sql: sql.to_string(),
            params: params.iter().map(|s| s.to_string()).collect(),
            reply: reply_tx,
        };
        self.send(cmd).await?;
        recv_reply(reply_rx).await?
    }

    /// Send an `Attach` command and await the reply. Consumer is the
    /// PR 3 `NamespaceManager::ensure_app_schema` impl.
    #[allow(dead_code)]
    pub(crate) async fn attach(&self, app_id: &str, db_path: &str) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<(), DbError>>(1);
        let cmd = Command::Attach {
            app_id: app_id.to_string(),
            db_path: db_path.to_string(),
            reply: reply_tx,
        };
        self.send(cmd).await?;
        recv_reply(reply_rx).await?
    }

    /// Send a `Detach` command and await the reply.
    #[allow(dead_code)]
    pub(crate) async fn detach(&self, app_id: &str) -> Result<(), DbError> {
        let (reply_tx, reply_rx) = flume::bounded::<Result<(), DbError>>(1);
        let cmd = Command::Detach {
            app_id: app_id.to_string(),
            reply: reply_tx,
        };
        self.send(cmd).await?;
        recv_reply(reply_rx).await?
    }

    async fn send(&self, cmd: Command) -> Result<(), DbError> {
        self.tx.send_async(cmd).await.map_err(|_| {
            DbError::internal("SqliteSession: worker thread is dead (queue receiver dropped)")
        })
    }
}

async fn recv_reply<T>(rx: flume::Receiver<T>) -> Result<T, DbError> {
    rx.recv_async().await.map_err(|_| {
        DbError::internal(
            "SqliteSession: worker dropped reply channel before producing a result",
        )
    })
}

impl Drop for SqliteSession {
    fn drop(&mut self) {
        // Best-effort: tell the worker to break out of its loop. If
        // the queue is full (highly unlikely on shutdown) or the
        // worker has already exited, ignore — the worker will also
        // observe the sender drop and end the loop naturally.
        let _ = self.tx.try_send(Command::Shutdown);
        // `_worker: std::thread::JoinHandle<()>` drops here without
        // joining. Dropping a `JoinHandle` detaches the thread; the
        // OS thread keeps running until the closure returns. The
        // `Shutdown` command above (or the `rx`-side disconnect when
        // `self.tx` drops with `self`) terminates the loop and the
        // thread exits + cleans up its connection on its own. We
        // deliberately do not `.join()` here because that would
        // block the dropping context, and the SQL the worker is
        // processing at this moment will commit (WAL-durable) or
        // roll back before the loop ends regardless.
    }
}

/// Clone-cheap handle to a [`SqliteSession`]. Wraps `Rc<SqliteSession>`
/// so consumers can hold many handles without paying for atomic
/// reference-counting (the compio runtime is single-threaded per
/// worker).
///
/// **`SqlExecutor::Client` association**: `SqliteBackend` reports
/// this type as its `Client` associated type. The PG backend's
/// `Client` is a real per-connection handle; SQLite has no
/// per-connection notion — the actor IS the only writer — so the
/// "client" is just a refcounted pointer to the same session every
/// other "client" already points at. Long-lived transactions
/// multiplex through the same mpsc queue and serialise by
/// construction.
#[derive(Clone)]
pub struct SqliteSessionHandle(pub(crate) Rc<SqliteSession>);

impl std::fmt::Debug for SqliteSessionHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SqliteSessionHandle").finish()
    }
}

impl SqliteSessionHandle {
    /// Wrap an existing session in a handle.
    #[allow(dead_code)]
    pub(crate) fn new(session: Rc<SqliteSession>) -> Self {
        Self(session)
    }

    /// Convenience: forward an `exec` through the underlying session.
    /// `SqliteBackend::client_exec` routes here.
    pub(crate) async fn exec(&self, sql: &str, params: &[&str]) -> Result<u64, DbError> {
        self.0.exec(sql, params).await
    }

    /// Forward a `query` through the underlying session.
    ///
    /// `pub` under the `test-helpers` feature so the integration
    /// target (`tests/sqlite_integration.rs`) can read PRAGMA values
    /// back without reaching into the actor surface directly. PR 4
    /// will route the SchemaIntrospect impl through this same path,
    /// at which point the visibility tightens back to `pub(crate)`.
    #[cfg(feature = "test-helpers")]
    pub async fn query(&self, sql: &str, params: &[&str]) -> Result<Vec<Row>, DbError> {
        self.0.query(sql, params).await
    }

    /// **P5 PR 3.5** — forward a `query_typed` through the underlying
    /// session. `pub` under the `test-helpers` feature so the P5 PR 3.5
    /// end-to-end encrypted-column round-trip test in
    /// `tests/sqlite_integration.rs` can read BLOB columns back as raw
    /// bytes without the `<N bytes blob>` stringification `query`
    /// emits.
    #[cfg(feature = "test-helpers")]
    pub async fn query_typed(
        &self,
        sql: &str,
        params: &[&str],
    ) -> Result<TypedRows, DbError> {
        self.0.query_typed(sql, params).await
    }
}

impl From<Rc<SqliteSession>> for SqliteSessionHandle {
    fn from(s: Rc<SqliteSession>) -> Self {
        Self(s)
    }
}

// ---------------------------------------------------------------------------
// Worker-side sync helpers — each runs inside the `spawn_blocking`
// thread, owns a `&Connection`, and consumes the owned param strings
// produced by the async `send`-side helpers.
// ---------------------------------------------------------------------------

fn run_exec(conn: &Connection, sql: &str, params: &[String]) -> Result<u64, DbError> {
    // **P5 PR 3.5** — the param vector carries an optional encrypted-
    // column side-channel: a value tagged with [`SQLITE_ENC_BLOB_PREFIX`]
    // is base64-decoded to raw bytes and bound as BLOB instead of TEXT.
    // The PG arm never produces this prefix; non-encrypted params
    // travel as plain `String` on both arms.
    let decoded = decode_blob_params(params)?;
    let refs: Vec<&dyn rusqlite::ToSql> = decoded
        .iter()
        .map(|p| p.as_to_sql())
        .collect();
    let n = conn
        .execute(sql, refs.as_slice())
        .map_err(from_sqlite)?;
    Ok(n as u64)
}

/// **P5 PR 3.5** — typed bind value. Either a borrowed `&str` (the
/// TEXT default — preserves the zero-alloc shape that
/// `&[String] → &[&dyn ToSql]` had pre-PR-3.5) or an owned `Vec<u8>`
/// produced by base64-decoding a [`SQLITE_ENC_BLOB_PREFIX`]-tagged
/// param.
enum BindParam<'a> {
    /// Plain TEXT bind — borrows from the caller's `Vec<String>`.
    Text(&'a str),
    /// BLOB bind — owns the decoded bytes.
    Blob(Vec<u8>),
}

impl BindParam<'_> {
    fn as_to_sql(&self) -> &dyn rusqlite::ToSql {
        match self {
            Self::Text(s) => s as &dyn rusqlite::ToSql,
            Self::Blob(v) => v as &dyn rusqlite::ToSql,
        }
    }
}

/// **P5 PR 3.5** — scan the param vector for encrypted-column side-
/// channel markers and produce a typed bind list. Values prefixed with
/// [`SQLITE_ENC_BLOB_PREFIX`] are base64-decoded to raw bytes and bound
/// as BLOB; every other value passes through as TEXT.
fn decode_blob_params(params: &[String]) -> Result<Vec<BindParam<'_>>, DbError> {
    use base64::Engine as _;
    let prefix = crate::query::SQLITE_ENC_BLOB_PREFIX;
    let mut out = Vec::with_capacity(params.len());
    for p in params {
        match p.strip_prefix(prefix) {
            Some(b64) => {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(b64)
                    .map_err(|e| {
                        DbError::internal(format!(
                            "sqlite session: encrypted-column param is not valid base64: {e}"
                        ))
                    })?;
                out.push(BindParam::Blob(bytes));
            }
            None => out.push(BindParam::Text(p.as_str())),
        }
    }
    Ok(out)
}

fn run_query(
    conn: &Connection,
    sql: &str,
    params: &[String],
) -> Result<Vec<Row>, DbError> {
    let mut stmt = conn.prepare(sql).map_err(from_sqlite)?;
    let column_count = stmt.column_count();
    // **P5 PR 3.5** — same encrypted-column blob-bind side-channel as
    // `run_exec` (see [`decode_blob_params`]).
    let decoded = decode_blob_params(params)?;
    let refs: Vec<&dyn rusqlite::ToSql> = decoded
        .iter()
        .map(|p| p.as_to_sql())
        .collect();
    let mut rows = stmt.query(refs.as_slice()).map_err(from_sqlite)?;
    let mut out = Vec::<Row>::new();
    while let Some(row) = rows.next().map_err(from_sqlite)? {
        let mut cells = Vec::with_capacity(column_count);
        for i in 0..column_count {
            // Materialise each cell as `Option<String>` regardless of
            // the underlying storage class. SQLite's typing is
            // dynamic — a single column can hold INTEGER / REAL /
            // TEXT / BLOB / NULL — so we inspect the `ValueRef`
            // discriminant and stringify uniformly. NULL → None;
            // everything else → Some(...).
            //
            // The Query path is consumed at PR 2 by PRAGMA inspection
            // (integration tests) and at PR 4 by the SchemaIntrospect
            // PRAGMA walk; both produce INTEGER + TEXT, never BLOB.
            // The BLOB arm uses `format!("{:?}", bytes)` so an
            // accidental binary column shows up as something the
            // operator can spot in logs without panicking the row
            // decoder.
            use rusqlite::types::ValueRef;
            let value_ref = row.get_ref(i).map_err(from_sqlite)?;
            let cell = match value_ref {
                ValueRef::Null => None,
                ValueRef::Integer(n) => Some(n.to_string()),
                ValueRef::Real(f) => Some(f.to_string()),
                ValueRef::Text(bytes) => Some(
                    std::str::from_utf8(bytes)
                        .map_err(|e| DbError::internal(format!(
                            "sqlite TEXT cell is not valid UTF-8: {e}"
                        )))?
                        .to_string(),
                ),
                ValueRef::Blob(bytes) => Some(format!("<{} bytes blob>", bytes.len())),
            };
            cells.push(cell);
        }
        out.push(cells);
    }
    Ok(out)
}

/// Typed row materialisation — the **P4 PR 4** vector path's
/// row-decoder. Preserves SQLite's storage-class discriminator so the
/// BLOB column reaches the caller as `Vec<u8>` (not the `<N bytes
/// blob>` placeholder string `run_query` emits at session.rs:522).
fn run_query_typed(
    conn: &Connection,
    sql: &str,
    params: &[String],
) -> Result<TypedRows, DbError> {
    let mut stmt = conn.prepare(sql).map_err(from_sqlite)?;
    let column_count = stmt.column_count();
    // `column_names` borrows from the statement; copy to owned `String`
    // BEFORE the row loop so the reply doesn't borrow from `stmt`.
    let columns: Vec<String> = (0..column_count)
        .map(|i| stmt.column_name(i).unwrap_or("").to_string())
        .collect();
    // **P5 PR 3.5** — same encrypted-column blob-bind side-channel as
    // `run_exec` (see [`decode_blob_params`]).
    let decoded = decode_blob_params(params)?;
    let refs: Vec<&dyn rusqlite::ToSql> = decoded
        .iter()
        .map(|p| p.as_to_sql())
        .collect();
    let mut rows = stmt.query(refs.as_slice()).map_err(from_sqlite)?;
    let mut out = Vec::<Vec<TypedCell>>::new();
    while let Some(row) = rows.next().map_err(from_sqlite)? {
        let mut cells = Vec::with_capacity(column_count);
        for i in 0..column_count {
            use rusqlite::types::ValueRef;
            let value_ref = row.get_ref(i).map_err(from_sqlite)?;
            let cell = match value_ref {
                ValueRef::Null => TypedCell::Null,
                ValueRef::Integer(n) => TypedCell::Integer(n),
                ValueRef::Real(f) => TypedCell::Real(f),
                ValueRef::Text(bytes) => TypedCell::Text(
                    std::str::from_utf8(bytes)
                        .map_err(|e| {
                            DbError::internal(format!(
                                "sqlite TEXT cell is not valid UTF-8: {e}"
                            ))
                        })?
                        .to_string(),
                ),
                // Copy the BLOB bytes into an owned `Vec<u8>` so the
                // reply crossing the actor boundary doesn't borrow from
                // rusqlite's statement-owned buffer.
                ValueRef::Blob(bytes) => TypedCell::Blob(bytes.to_vec()),
            };
            cells.push(cell);
        }
        out.push(cells);
    }
    Ok(TypedRows {
        columns,
        rows: out,
    })
}

fn run_attach(conn: &Connection, app_id: &str, db_path: &str) -> Result<(), DbError> {
    // ATTACH does not accept bind parameters for the path or alias —
    // both are SQL syntax. The PR 3 NamespaceManager impl validates
    // `app_id` upstream (it's already constrained to `[A-Za-z0-9_]`
    // by the per-app schema convention), so the only safe way to
    // construct the statement is via formatted SQL with quoted
    // literals. We use SQLite's standard double-quote-on-identifier
    // and single-quote-on-string convention.
    //
    // PR 3 will move this string-building into `SqliteDialect`; PR 2
    // ships the helper so the actor can serve the variant.
    let escaped_path = db_path.replace('\'', "''");
    let escaped_alias = app_id.replace('"', "\"\"");
    let sql = format!(
        "ATTACH DATABASE 'file:{escaped_path}' AS \"{escaped_alias}\""
    );
    conn.execute_batch(&sql).map_err(from_sqlite)
}

fn run_detach(conn: &Connection, app_id: &str) -> Result<(), DbError> {
    let escaped_alias = app_id.replace('"', "\"\"");
    let sql = format!("DETACH DATABASE \"{escaped_alias}\"");
    conn.execute_batch(&sql).map_err(from_sqlite)
}
