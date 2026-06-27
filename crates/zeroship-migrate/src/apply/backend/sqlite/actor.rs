//! The dedicated, hardened, CDC-free migration connection actor (design §2.1.1).
//!
//! `SqliteBackend` does NOT reuse plugin-db's data-plane `SqliteSession` (which
//! auto-loads `vec0` process-globally, installs CDC hooks, and multiplexes the
//! data plane). It owns a **thin migration-only sibling actor**: the same
//! zero-tokio shape (a dedicated OS thread owns the single `rusqlite::Connection`
//! and drains a `flume` queue; callers `await` a `flume` reply), but the
//! connection is opened with the **migration-hardening profile** (§2.5.1) and is
//! migration-private (no CDC, no data-plane sharing).
//!
//! # Open sequence (§2.5.1, exact order)
//!
//! ```text
//! 0. open the APP FILE as the connection's MAIN db (the creator `up` lands here)
//! 0. ATTACH 'file:<journal>'     AS "_mig"   -- engine, BEFORE the authorizer
//! 1. PRAGMA foreign_keys = ON                -- engine-set, the only PRAGMA
//! 2. conn.load_extension_disable()           -- real rusqlite API (not a DbConfig)
//! 3. set_db_config(DEFENSIVE, true)
//! 4. set_db_config(TRUSTED_SCHEMA, false)
//! 5. set_db_config(DQS_DDL, false)
//! 6. set_db_config(DQS_DML, false)
//! 7. conn.authorizer(Some(callback))         -- LAST, before any creator SQL
//! ```
//!
//! After step 7, ATTACH/DETACH are denied for life — cross-tenant is closed by
//! construction (§2.5.2). The ONLY two databases on this connection are `main`
//! (the app file) and `_mig` (the journal file), each opened exactly once.
//!
//! # Why `main` IS the app file (not `:memory:`, not a double-ATTACH)
//!
//! The app file is the connection's **main** database, so an UNqualified creator
//! `CREATE TABLE users(...)` lands in — and PERSISTS to — the app file. We do NOT
//! ATTACH the app file a second time: opening the same file as both `main` and an
//! `app` alias would open it TWICE on one connection, and `BEGIN IMMEDIATE` would
//! deadlock the two handles against each other on the file's RESERVED lock. With
//! `main` = the app file there is exactly ONE handle on it, plus ONE on `_mig`, so
//! a single-connection `BEGIN IMMEDIATE` takes their RESERVED locks cleanly.

use std::path::{Path, PathBuf};

use rusqlite::config::DbConfig;
use rusqlite::Connection;

use super::authorizer::{make_authorizer, AuthMode, Mode, MIG_ALIAS};

/// The version floor the journal-immutability + feature set requires (§2.9):
/// DEFENSIVE/TRUSTED_SCHEMA (≥3.31/3.26), DQS dbconfig (≥3.29), RETURNING (≥3.35),
/// window functions (≥3.25), and the authorizer passing `zDb` on DROP_TABLE/DML.
/// Bundled SQLite is 3.51.3; the check refuses to run below the floor so the
/// deny-on-`_mig` proof cannot silently no-op against an exotic build (§2.9).
const SQLITE_VERSION_FLOOR: i32 = 3_035_000; // 3.35.0 — the highest single floor (RETURNING)

/// An error from the migration SQLite actor. Dialect-neutral `String` payloads so
/// it can flow through the generic backend errors (which add a `Backend(String)`
/// arm) without leaking a PG error type.
#[derive(Debug, thiserror::Error)]
pub enum SqliteActorError {
    /// Connection open / hardening / attach failed.
    #[error("sqlite migration connection open failed: {0}")]
    Open(String),
    /// The linked SQLite is below the supported version floor (§2.9).
    #[error("unsupported sqlite version {found}: migration engine requires >= {floor} (3.35.0)")]
    UnsupportedVersion { found: i32, floor: i32 },
    /// A statement failed (prepare/step). A prepare-time authorizer DENY surfaces
    /// here too (`SQLITE_AUTH` / "not authorized").
    #[error("sqlite migration statement failed: {0}")]
    Exec(String),
    /// The long-lived migration connection is wedged: a failed `up` left a
    /// transaction open (ROLLBACK errored, or the connection is not back in
    /// autocommit). The connection can no longer be safely reused; the caller must
    /// tear it down and rebuild before the next apply (H1).
    #[error("sqlite migration connection poisoned (transaction not cleanly rolled back): {0}")]
    Poisoned(String),
    /// The actor thread died / the queue disconnected.
    #[error("sqlite migration actor unavailable: {0}")]
    Unavailable(String),
}

impl SqliteActorError {
    /// True iff this error is an authorizer DENY (`SQLITE_AUTH`). The confinement
    /// tests assert a denied attack surfaces as this (not a corruption / silent
    /// pass). rusqlite renders a denied prepare as a `SqliteFailure` with
    /// `ErrorCode::AuthorizationForStatementDenied` and the message "not
    /// authorized" for STATEMENT-level denials. A denied `Read` (a column read)
    /// renders differently: the message contains `is prohibited`
    /// (still `SQLITE_AUTH`, the authorizer's column-read deny path, M1) — so we
    /// match that wording too, else a legitimate authorizer deny of a creator
    /// `SELECT FROM "_mig"` would be misclassified as an unrelated error.
    #[must_use]
    pub fn is_authorizer_denied(&self) -> bool {
        match self {
            SqliteActorError::Exec(m) => {
                let l = m.to_ascii_lowercase();
                l.contains("not authorized")
                    || l.contains("authorization")
                    || l.contains("is prohibited")
            }
            _ => false,
        }
    }
}

/// A single migration-actor command. Each carries a `flume::bounded(1)` reply.
enum Command {
    /// Run one statement (prepare+step under the current authorizer mode). Used
    /// for the creator `up` AND the journal writes — NEVER a multi-statement batch
    /// that spans a mode boundary (§2.2.2: the mode is read at prepare time, so
    /// each prepare must happen under its intended mode).
    Exec {
        sql: String,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Run one query, returning text rows (every cell stringified so the reply is
    /// `Send`). Used for journal net-state reads + the event-seq allocation
    /// (`UPDATE ... RETURNING`).
    Query {
        sql: String,
        reply: flume::Sender<Result<Vec<Vec<Option<String>>>, SqliteActorError>>,
    },
    /// **PR6a** — run ONE parameterized DML statement, binding `params` NATIVELY
    /// to the `?n` placeholders (never string-interpolated). The single source of
    /// the creator one-shot DML SQLite executor (`run_dml_step`): the assembler
    /// renders the `?n` template + the typed binds, and they are bound here so a
    /// bind value can never alter the statement shape (§2.3.2 bind-safety). Prepared
    /// + stepped under the current authorizer mode, exactly like [`Command::Exec`].
    ExecParams {
        sql: String,
        params: Vec<SqliteBind>,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// **PR6b** — run ONE parameterized statement that returns rows (a windowed
    /// backfill `UPDATE … RETURNING <cursor>`, or a parameterized SELECT), binding
    /// `params` NATIVELY to its `?n` placeholders. The single source the SQLite
    /// batched-backfill executor uses to run a batch's `UPDATE … RETURNING` and read
    /// back the touched cursor values (§2.3.1) — never string-interpolated, so a
    /// bind value can never alter the statement shape. Prepared + stepped under the
    /// current authorizer mode, exactly like [`Command::Query`].
    QueryParams {
        sql: String,
        params: Vec<SqliteBind>,
        reply: flume::Sender<Result<Vec<Vec<Option<String>>>, SqliteActorError>>,
    },
    /// Flip the authorizer mode (§2.2.2). A plain atomic store on the worker side,
    /// between (never inside) statement prepares.
    SetMode {
        mode: Mode,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Report whether the connection is in autocommit mode (no open transaction).
    /// Used to detect a wedged connection after a failed `up` + ROLLBACK (H1).
    IsAutocommit {
        reply: flume::Sender<bool>,
    },
    /// Stop the worker (drops the connection, WAL-checkpoints on close).
    Shutdown,
}

/// **PR6a** — a typed value bound NATIVELY to a `?n` placeholder of a one-shot
/// DML statement. A transport-safe (`Send`) mirror of [`crate::render::step::BindValue`]
/// the actor binds via rusqlite's parameter API — never interpolated, so a bind
/// value can never alter the statement shape (§2.3.2). `Decimal`/`Text` are bound
/// as TEXT (the column affinity coerces; the IR numeric domain is i64 +
/// decimal-string).
#[derive(Debug, Clone)]
pub enum SqliteBind {
    /// SQL `NULL`.
    Null,
    /// A boolean (bound as the integer 0/1, SQLite's boolean representation).
    Bool(bool),
    /// An exact 64-bit integer.
    Int(i64),
    /// A decimal/text value bound as TEXT (affinity coerces).
    Text(String),
}

impl SqliteBind {
    /// Map a plan [`BindValue`](crate::render::step::BindValue) to its transport mirror.
    #[must_use]
    pub fn from_bind(b: &crate::render::step::BindValue) -> Self {
        match b {
            crate::render::step::BindValue::Null => SqliteBind::Null,
            crate::render::step::BindValue::Bool(v) => SqliteBind::Bool(*v),
            crate::render::step::BindValue::Int(v) => SqliteBind::Int(*v),
            crate::render::step::BindValue::Decimal(s) => SqliteBind::Text(s.clone()),
            crate::render::step::BindValue::Text(s) => SqliteBind::Text(s.clone()),
        }
    }

    fn to_sql_value(&self) -> rusqlite::types::Value {
        use rusqlite::types::Value;
        match self {
            SqliteBind::Null => Value::Null,
            SqliteBind::Bool(b) => Value::Integer(i64::from(*b)),
            SqliteBind::Int(i) => Value::Integer(*i),
            SqliteBind::Text(s) => Value::Text(s.clone()),
        }
    }
}

/// The migration actor handle. Cloneable-free (one owner): the backend holds it.
#[derive(Debug)]
pub struct MigrationActor {
    tx: flume::Sender<Command>,
    _worker: std::thread::JoinHandle<()>,
}

impl MigrationActor {
    /// Open the hardened migration connection for one tenant.
    ///
    /// `app_path` is opened as the connection's MAIN database (the creator `up`
    /// lands here and persists). `journal_path` is ATTACHed as `"_mig"`, BEFORE the
    /// authorizer is installed (§2.5.1 step 0). The two files are constructed by the
    /// engine from the authenticated `app_id`, never from creator input (§2.5.2).
    ///
    /// # Errors
    /// [`SqliteActorError::Open`] / [`SqliteActorError::UnsupportedVersion`] on a
    /// failed open, attach, hardening step, or sub-floor SQLite.
    pub fn open(app_path: &Path, journal_path: &Path) -> Result<Self, SqliteActorError> {
        let (tx, rx) = flume::bounded::<Command>(64);
        let (startup_tx, startup_rx) = flume::bounded::<Result<(), SqliteActorError>>(1);

        let app_path: PathBuf = app_path.to_path_buf();
        let journal_path: PathBuf = journal_path.to_path_buf();

        let worker = std::thread::Builder::new()
            .name("zs-migrate-sqlite".to_string())
            .spawn(move || {
                // Build the hardened connection. Any failure is reported via the
                // startup channel; the worker then exits without serving commands.
                let conn = match open_hardened(&app_path, &journal_path) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = startup_tx.send(Err(e));
                        return;
                    }
                };
                let _ = startup_tx.send(Ok(()));

                // Command loop. The single connection serializes every statement;
                // the mode flag lives in the captured authorizer closure (already
                // installed inside `open_hardened`) and is flipped via SetMode.
                while let Ok(cmd) = rx.recv() {
                    match cmd {
                        Command::Exec { sql, reply } => {
                            let _ = reply.send(run_exec(&conn, &sql));
                        }
                        Command::Query { sql, reply } => {
                            let _ = reply.send(run_query(&conn, &sql));
                        }
                        Command::ExecParams { sql, params, reply } => {
                            let _ = reply.send(run_exec_params(&conn, &sql, &params));
                        }
                        Command::QueryParams { sql, params, reply } => {
                            let _ = reply.send(run_query_params(&conn, &sql, &params));
                        }
                        Command::SetMode { mode, reply } => {
                            // The authorizer mode flag is owned by the closure; we
                            // flip it through the shared AuthMode handle stashed on
                            // the connection's user-data. We instead keep the handle
                            // alongside the connection via a thread-local-free design:
                            // see `HardenedConn`.
                            conn.flip_mode(mode);
                            let _ = reply.send(Ok(()));
                        }
                        Command::IsAutocommit { reply } => {
                            let _ = reply.send(conn.is_autocommit());
                        }
                        Command::Shutdown => break,
                    }
                }
            })
            .map_err(|e| SqliteActorError::Open(format!("failed to spawn migration actor: {e}")))?;

        match startup_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                _worker: worker,
            }),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(SqliteActorError::Open(
                "migration actor exited before startup signal".to_string(),
            )),
        }
    }

    async fn send(&self, cmd: Command) -> Result<(), SqliteActorError> {
        self.tx
            .send_async(cmd)
            .await
            .map_err(|_| SqliteActorError::Unavailable("actor queue closed".to_string()))
    }

    /// Run one statement under the current authorizer mode.
    pub async fn exec(&self, sql: &str) -> Result<(), SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::Exec {
            sql: sql.to_string(),
            reply,
        })
        .await?;
        recv(rx).await?
    }

    /// **PR6a** — run ONE parameterized DML statement, binding `params` natively to
    /// its `?n` placeholders. The creator one-shot DML SQLite executor — the binds
    /// can never alter the statement shape (§2.3.2).
    ///
    /// # Errors
    /// [`SqliteActorError::Exec`] on a prepare/step failure (an authorizer DENY
    /// surfaces here too); [`SqliteActorError::Unavailable`] if the actor died.
    pub async fn exec_params(
        &self,
        sql: &str,
        params: &[SqliteBind],
    ) -> Result<(), SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::ExecParams {
            sql: sql.to_string(),
            params: params.to_vec(),
            reply,
        })
        .await?;
        recv(rx).await?
    }

    /// **PR6b** — run ONE parameterized statement that returns rows (a windowed
    /// backfill `UPDATE … RETURNING <cursor>`, or a parameterized SELECT), binding
    /// `params` natively to its `?n` placeholders. The batched-backfill executor's
    /// per-batch primitive — the binds can never alter the statement shape (§2.3.1).
    ///
    /// # Errors
    /// [`SqliteActorError::Exec`] on a prepare/step failure (an authorizer DENY
    /// surfaces here too); [`SqliteActorError::Unavailable`] if the actor died.
    pub async fn query_params(
        &self,
        sql: &str,
        params: &[SqliteBind],
    ) -> Result<Vec<Vec<Option<String>>>, SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::QueryParams {
            sql: sql.to_string(),
            params: params.to_vec(),
            reply,
        })
        .await?;
        recv(rx).await?
    }

    /// Run one query, returning stringified rows.
    pub async fn query(&self, sql: &str) -> Result<Vec<Vec<Option<String>>>, SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::Query {
            sql: sql.to_string(),
            reply,
        })
        .await?;
        recv(rx).await?
    }

    /// Flip the authorizer mode (§2.2.2). Between (never inside) statement
    /// prepares — enforced by the single-connection actor serialization.
    pub async fn set_mode(&self, mode: Mode) -> Result<(), SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::SetMode { mode, reply }).await?;
        recv(rx).await?
    }

    /// Whether the connection is in autocommit mode (i.e. NO open transaction).
    /// After a failed `up` + ROLLBACK this MUST be `true`; a `false` means the
    /// transaction is still open and the long-lived connection is wedged (H1).
    pub async fn is_autocommit(&self) -> Result<bool, SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::IsAutocommit { reply }).await?;
        recv(rx).await
    }
}

impl Drop for MigrationActor {
    fn drop(&mut self) {
        let _ = self.tx.send(Command::Shutdown);
    }
}

async fn recv<T>(rx: flume::Receiver<T>) -> Result<T, SqliteActorError> {
    rx.recv_async()
        .await
        .map_err(|_| SqliteActorError::Unavailable("migration actor dropped reply".to_string()))
}

/// A `rusqlite::Connection` plus the [`AuthMode`] handle it shares with its
/// installed authorizer closure. Owns the mode-flip seam so the actor can switch
/// CreatorUp ↔ EngineJournal without re-installing the closure (§2.2.2).
struct HardenedConn {
    conn: Connection,
    mode: AuthMode,
}

impl HardenedConn {
    fn flip_mode(&self, mode: Mode) {
        self.mode.store(mode);
    }
}

// Allow the actor loop to call `conn.flip_mode(..)` / pass `&conn` to run_* by
// deref-ing the wrapper to the inner Connection where a `&Connection` is needed.
impl std::ops::Deref for HardenedConn {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.conn
    }
}

/// Open + harden the connection per §2.5.1 (exact order). Returns the wrapper that
/// keeps the [`AuthMode`] alive alongside the connection.
fn open_hardened(app_path: &Path, journal_path: &Path) -> Result<HardenedConn, SqliteActorError> {
    // Version floor (§2.9) — refuse to run below the supported SQLite.
    let v = rusqlite::version_number();
    if v < SQLITE_VERSION_FLOOR {
        return Err(SqliteActorError::UnsupportedVersion {
            found: v,
            floor: SQLITE_VERSION_FLOOR,
        });
    }

    // 0. Open the APP FILE as the connection's MAIN database, then ATTACH ONLY the
    //    journal file as `_mig`. The app file is NOT ATTACHed a second time:
    //    attaching a file that is also the main DB would open the SAME file twice on
    //    one connection, and `BEGIN IMMEDIATE` would deadlock the two handles against
    //    each other on the file's RESERVED lock. With `main` = the app file, the only
    //    databases are `main` (app) and `_mig`, each opened exactly once, so a
    //    single-connection `BEGIN IMMEDIATE` takes their RESERVED locks cleanly.
    //
    //    Because `main` is the app file, an UNqualified creator `CREATE TABLE
    //    users(...)` lands in — and PERSISTS to — the app file. NOTE for P4: the
    //    SQLite migration `up` MUST be emitted UNqualified (no `"<app_id>".table`
    //    schema prefix) so it targets `main`. The shared emitter currently qualifies
    //    as `"<app_id>".table` (a PG-schema shape); wiring the SQLite emitter to emit
    //    unqualified DDL is the P4 emitter-alias reconciliation. The authorizer
    //    treats `main` (and the bare/None database) as the creator-writable target.
    let conn = Connection::open(app_path)
        .map_err(|e| SqliteActorError::Open(format!("open main (app file): {e}")))?;

    // ATTACH the journal AS "_mig" — BEFORE the authorizer. The path comes from the
    // authenticated app_id, never creator input. Bind it as a parameter so the
    // filename is never interpolated into SQL.
    conn.execute(
        &format!("ATTACH DATABASE ?1 AS \"{MIG_ALIAS}\""),
        [path_str(journal_path)?],
    )
    .map_err(|e| SqliteActorError::Open(format!("attach _mig: {e}")))?;

    // 1. Engine-set PRAGMAs, applied at open BEFORE the authorizer (which denies
    //    PRAGMA for the connection's life). `busy_timeout` gives a bounded wait so a
    //    transient internal lock (e.g. between the RETURNING read and the next write
    //    on the same connection across `main` + `_mig`) does not surface as an
    //    immediate `SQLITE_BUSY`. `foreign_keys=ON` enables FK enforcement (a
    //    per-connection setting SQLite ships OFF). WAL is NOT set here: the
    //    migration connection is the single writer (in-process serialization, §2.3),
    //    and the default rollback journal is simplest for the atomic apply; the
    //    busy_timeout covers the only contention (the connection with itself across
    //    `main` + `_mig` during BEGIN IMMEDIATE).
    conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
        .map_err(|e| SqliteActorError::Open(format!("pragma bootstrap: {e}")))?;

    // 2. Disable extension loading (real rusqlite API; not a DbConfig variant).
    conn.load_extension_disable()
        .map_err(|e| SqliteActorError::Open(format!("load_extension_disable: {e}")))?;

    // 3-6. The hardening dbconfig profile.
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)
        .map_err(|e| SqliteActorError::Open(format!("dbconfig DEFENSIVE: {e}")))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA, false)
        .map_err(|e| SqliteActorError::Open(format!("dbconfig TRUSTED_SCHEMA: {e}")))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DDL, false)
        .map_err(|e| SqliteActorError::Open(format!("dbconfig DQS_DDL: {e}")))?;
    conn.set_db_config(DbConfig::SQLITE_DBCONFIG_DQS_DML, false)
        .map_err(|e| SqliteActorError::Open(format!("dbconfig DQS_DML: {e}")))?;

    // 7. Install the authorizer LAST, before any creator SQL. The mode defaults to
    //    the most-restrictive CreatorUp; the engine opts into EngineJournal only
    //    for its own bootstrap + journal writes.
    let mode = AuthMode::new();
    conn.authorizer(Some(make_authorizer(mode.clone())))
        .map_err(|e| SqliteActorError::Open(format!("install authorizer: {e}")))?;

    Ok(HardenedConn { conn, mode })
}

fn path_str(p: &Path) -> Result<String, SqliteActorError> {
    p.to_str()
        .map(str::to_string)
        .ok_or_else(|| SqliteActorError::Open(format!("non-utf8 db path: {}", p.display())))
}

/// Run one statement (prepare + step). The authorizer fires at prepare under the
/// current mode; a DENY surfaces as a `SqliteFailure` rendered into
/// [`SqliteActorError::Exec`]. We use `execute_batch` for a single statement so
/// PRAGMA/DDL that return no rows are handled uniformly — but callers MUST pass a
/// single statement (a mode-spanning batch would be prepared under one mode).
fn run_exec(conn: &Connection, sql: &str) -> Result<(), SqliteActorError> {
    conn.execute_batch(sql)
        .map_err(|e| SqliteActorError::Exec(e.to_string()))
}

/// **PR6a** — run ONE parameterized statement, binding `params` natively to its
/// `?n` placeholders (rusqlite `execute` with positional params). Prepared under
/// the current authorizer mode (a DENY surfaces as [`SqliteActorError::Exec`]).
/// MUST be a single statement (the mode is read at prepare).
fn run_exec_params(
    conn: &Connection,
    sql: &str,
    params: &[SqliteBind],
) -> Result<(), SqliteActorError> {
    let values: Vec<rusqlite::types::Value> = params.iter().map(SqliteBind::to_sql_value).collect();
    let refs: Vec<&dyn rusqlite::ToSql> =
        values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    conn.execute(sql, refs.as_slice())
        .map(|_| ())
        .map_err(|e| SqliteActorError::Exec(e.to_string()))
}

/// **PR6b** — run ONE parameterized statement that returns rows, binding `params`
/// natively to its `?n` placeholders, stringifying every cell for transport.
/// Prepared under the current authorizer mode (a DENY surfaces as
/// [`SqliteActorError::Exec`]). MUST be a single statement.
fn run_query_params(
    conn: &Connection,
    sql: &str,
    params: &[SqliteBind],
) -> Result<Vec<Vec<Option<String>>>, SqliteActorError> {
    let values: Vec<rusqlite::types::Value> = params.iter().map(SqliteBind::to_sql_value).collect();
    let refs: Vec<&dyn rusqlite::ToSql> =
        values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| SqliteActorError::Exec(e.to_string()))?;
    let col_count = stmt.column_count();
    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            let mut out = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let cell: Option<String> = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                    rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                    rusqlite::types::ValueRef::Text(t) => {
                        Some(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => Some(hex::encode(b)),
                };
                out.push(cell);
            }
            Ok(out)
        })
        .map_err(|e| SqliteActorError::Exec(e.to_string()))?;
    let mut materialized = Vec::new();
    for r in rows {
        materialized.push(r.map_err(|e| SqliteActorError::Exec(e.to_string()))?);
    }
    Ok(materialized)
}

/// Run one query, stringifying every cell so the reply crosses the actor boundary.
fn run_query(conn: &Connection, sql: &str) -> Result<Vec<Vec<Option<String>>>, SqliteActorError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|e| SqliteActorError::Exec(e.to_string()))?;
    let col_count = stmt.column_count();
    let rows = stmt
        .query_map([], |row| {
            let mut out = Vec::with_capacity(col_count);
            for i in 0..col_count {
                // Coerce every storage class to text (or NULL) for transport.
                let cell: Option<String> = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                    rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                    rusqlite::types::ValueRef::Text(t) => {
                        Some(String::from_utf8_lossy(t).into_owned())
                    }
                    rusqlite::types::ValueRef::Blob(b) => Some(hex::encode(b)),
                };
                out.push(cell);
            }
            Ok(out)
        })
        .map_err(|e| SqliteActorError::Exec(e.to_string()))?;
    let mut materialized = Vec::new();
    for r in rows {
        materialized.push(r.map_err(|e| SqliteActorError::Exec(e.to_string()))?);
    }
    Ok(materialized)
}
