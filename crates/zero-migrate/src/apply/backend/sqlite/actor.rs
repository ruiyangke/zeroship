//! The dedicated, hardened, CDC-free migration connection actor.
//!
//! `SqliteBackend` does NOT reuse plugin-db's data-plane `SqliteSession` (which
//! auto-loads `vec0` process-globally, installs CDC hooks, and multiplexes the
//! data plane). It owns a **thin migration-only sibling actor**: the same
//! zero-tokio shape (a dedicated OS thread owns the single `rusqlite::Connection`
//! and drains a `flume` queue; callers `await` a `flume` reply), but the
//! connection is opened with the **migration-hardening profile** and is
//! migration-private (no CDC, no data-plane sharing).
//!
//! # Open sequence
//!
//! ```text
//! 0. open the APP FILE as the connection's MAIN db (the creator `up` lands here)
//! 0. ATTACH an existing journal as `_mig`, or leave it unattached when absent
//! 1. pin both databases to DELETE rollback journals + FULL synchronous;
//!    PRAGMA foreign_keys = ON
//! 2. conn.load_extension_disable -- real rusqlite API (not a DbConfig)
//! 3. set_db_config(DEFENSIVE, true)
//! 4. set_db_config(TRUSTED_SCHEMA, false)
//! 5. set_db_config(DQS_DDL, false)
//! 6. set_db_config(DQS_DML, false)
//! 7. conn.authorizer(Some(callback)) -- LAST, before any creator SQL
//! ```
//!
//! After step 7, ATTACH/DETACH are denied for the connection's life. A fresh
//! read-only status connection therefore has only `main`. Journal bootstrap
//! replaces it with a newly hardened connection that attached `_mig` before its
//! authorizer was installed. A connection never executes ATTACH after hardening.
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

use super::authorizer::{make_authorizer, AuthMode, DenialLog, Mode, MIG_ALIAS};

/// The version floor the journal-immutability + feature set requires:
/// DEFENSIVE/TRUSTED_SCHEMA (≥3.31/3.26), DQS dbconfig (≥3.29), RETURNING (≥3.35),
/// window functions (≥3.25), and the authorizer passing `zDb` on DROP_TABLE/DML.
/// Bundled SQLite is 3.51.3; the check refuses to run below the floor so the
/// deny-on-`_mig` proof cannot silently no-op against an exotic build.
const SQLITE_VERSION_FLOOR: i32 = 3_035_000; // 3.35.0 — the highest single floor (RETURNING)

/// An error from the migration SQLite actor. Dialect-neutral `String` payloads so
/// it can flow through the generic backend errors (which add a `Backend(String)`
/// arm) without leaking a PG error type.
#[derive(Debug, thiserror::Error)]
pub enum SqliteActorError {
    /// Connection open / hardening / attach failed.
    #[error("sqlite migration connection open failed: {0}")]
    Open(String),
    /// The linked SQLite is below the supported version floor.
    #[error("unsupported sqlite version {found}: migration engine requires >= {floor} (3.35.0)")]
    UnsupportedVersion { found: i32, floor: i32 },
    /// A statement failed (prepare/step). A prepare-time authorizer DENY surfaces
    /// here too (`SQLITE_AUTH` / "not authorized").
    #[error("sqlite migration statement failed: {0}")]
    Exec(String),
    /// The long-lived migration connection is wedged: a failed `up` left a
    /// transaction open (ROLLBACK errored, or the connection is not back in
    /// autocommit). The connection can no longer be safely reused; the caller must
    /// tear it down and rebuild before the next apply.
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
    /// (still `SQLITE_AUTH`, the authorizer's column-read deny path) — so we
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
    /// that spans a mode boundary (the mode is read at prepare time, so
    /// each prepare must happen under its intended mode).
    Exec {
        sql: String,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Run one query, returning text rows (every cell stringified so the reply is
    /// `Send`). Used for journal net-state reads + the event-seq allocation
    /// (`UPDATE... RETURNING`).
    Query {
        sql: String,
        reply: flume::Sender<Result<Vec<Vec<Option<String>>>, SqliteActorError>>,
    },
    /// Run ONE parameterized DML statement, binding `params` NATIVELY
    /// to the `?n` placeholders (never string-interpolated). The single source of
    /// the creator one-shot DML SQLite executor (`run_dml_step`): the assembler
    /// renders the `?n` template + the typed binds, and they are bound here so a
    /// bind value can never alter the statement shape. Prepared
    /// + stepped under the current authorizer mode, exactly like [`Command::Exec`].
    ExecParams {
        sql: String,
        params: Vec<SqliteBind>,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Run ONE parameterized statement that returns rows (a windowed
    /// backfill `UPDATE … RETURNING <cursor>`, or a parameterized SELECT), binding
    /// `params` NATIVELY to its `?n` placeholders. The single source the SQLite
    /// batched-backfill executor uses to run a batch's `UPDATE … RETURNING` and read
    /// back the touched cursor values — never string-interpolated, so a
    /// bind value can never alter the statement shape. Prepared + stepped under the
    /// current authorizer mode, exactly like [`Command::Query`].
    QueryParams {
        sql: String,
        params: Vec<SqliteBind>,
        reply: flume::Sender<Result<Vec<Vec<Option<String>>>, SqliteActorError>>,
    },
    /// Stream one text column and verify that every value has a lossless UTF-8
    /// representation. Backfill checkpoints are Rust strings, so this preflight
    /// must finish before any batch is allowed to commit.
    ValidateTextUtf8 {
        sql: String,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Flip the authorizer mode. A plain atomic store on the worker side,
    /// between (never inside) statement prepares.
    SetMode {
        mode: Mode,
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Reopen the hardened connection with the journal attached. The reopen is
    /// the only way to preserve the invariant that `_mig` is attached before the
    /// authorizer is installed while still avoiding creation on read-only opens.
    EnsureJournalAttached {
        reply: flume::Sender<Result<(), SqliteActorError>>,
    },
    /// Report whether the connection is in autocommit mode (no open transaction).
    /// Used to detect a wedged connection after a failed `up` + ROLLBACK.
    IsAutocommit { reply: flume::Sender<bool> },
    /// Stop the worker (drops the connection, WAL-checkpoints on close).
    Shutdown,
}

/// A typed value bound NATIVELY to a `?n` placeholder of a one-shot
/// DML statement. A transport-safe (`Send`) mirror of [`crate::render::step::BindValue`]
/// the actor binds via rusqlite's parameter API — never interpolated, so a bind
/// value can never alter the statement shape. `Decimal`/`Text` are bound
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
    /// Exact binary value bound as a SQLite BLOB.
    Blob(Vec<u8>),
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
            crate::render::step::BindValue::Bytes(bytes) => SqliteBind::Blob(bytes.clone()),
        }
    }

    fn to_sql_value(&self) -> rusqlite::types::Value {
        use rusqlite::types::Value;
        match self {
            SqliteBind::Null => Value::Null,
            SqliteBind::Bool(b) => Value::Integer(i64::from(*b)),
            SqliteBind::Int(i) => Value::Integer(*i),
            SqliteBind::Text(s) => Value::Text(s.clone()),
            SqliteBind::Blob(bytes) => Value::Blob(bytes.clone()),
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
    /// lands here and persists). An existing `journal_path` is attached as `_mig`
    /// before the authorizer is installed. An absent journal is not attached or
    /// created until journal bootstrap explicitly requests it. Both paths are
    /// constructed by the engine from the authenticated `app_id`, never from
    /// creator input.
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
                let mut conn =
                    match open_hardened(&app_path, &journal_path, JournalAttachment::IfPresent) {
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
                        Command::ValidateTextUtf8 { sql, reply } => {
                            let _ = reply.send(run_validate_text_utf8(&conn, &sql));
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
                        Command::EnsureJournalAttached { reply } => {
                            let result = if conn.journal_attached {
                                Ok(())
                            } else {
                                match open_hardened(
                                    &app_path,
                                    &journal_path,
                                    JournalAttachment::Required,
                                ) {
                                    Ok(reopened) => {
                                        conn = reopened;
                                        Ok(())
                                    }
                                    Err(error) => Err(error),
                                }
                            };
                            let _ = reply.send(result);
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

    /// Run ONE parameterized DML statement, binding `params` natively to
    /// its `?n` placeholders. The creator one-shot DML SQLite executor — the binds
    /// can never alter the statement shape.
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

    /// Run ONE parameterized statement that returns rows (a windowed
    /// backfill `UPDATE … RETURNING <cursor>`, or a parameterized SELECT), binding
    /// `params` natively to its `?n` placeholders. The batched-backfill executor's
    /// per-batch primitive — the binds can never alter the statement shape.
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

    /// Stream a single TEXT expression and reject the first value that cannot be
    /// represented losslessly as UTF-8. No result rows are materialized.
    pub async fn validate_text_utf8(&self, sql: &str) -> Result<(), SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::ValidateTextUtf8 {
            sql: sql.to_string(),
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

    /// Flip the authorizer mode. Between (never inside) statement
    /// prepares — enforced by the single-connection actor serialization.
    pub async fn set_mode(&self, mode: Mode) -> Result<(), SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::SetMode { mode, reply }).await?;
        recv(rx).await?
    }

    /// Ensure the separate journal file is attached as `_mig`.
    ///
    /// A fresh actor deliberately starts without `_mig` when the journal file is
    /// absent. Bootstrap calls this seam to reopen the connection and attach the
    /// journal before installing the replacement connection's authorizer. Repeated
    /// calls are no-ops.
    pub(crate) async fn ensure_journal_attached(&self) -> Result<(), SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::EnsureJournalAttached { reply }).await?;
        recv(rx).await?
    }

    /// Whether the connection is in autocommit mode (i.e. NO open transaction).
    /// After a failed `up` + ROLLBACK this MUST be `true`; a `false` means the
    /// transaction is still open and the long-lived connection is wedged.
    pub async fn is_autocommit(&self) -> Result<bool, SqliteActorError> {
        let (reply, rx) = flume::bounded(1);
        self.send(Command::IsAutocommit { reply }).await?;
        recv(rx).await
    }

    /// Commit the current engine-owned transaction, cleaning it up before
    /// returning if `COMMIT` fails. SQLite can leave a transaction open after a
    /// deferred-constraint or I/O failure at commit time; returning that raw
    /// error would poison this long-lived connection for every later migration.
    ///
    /// A failed commit is followed by `ROLLBACK` and an autocommit probe. The
    /// original commit error is returned only when the connection is confirmed
    /// clean. Otherwise the stronger [`SqliteActorError::Poisoned`] error wins.
    pub(crate) async fn commit_or_cleanup(&self, context: &str) -> Result<(), SqliteActorError> {
        self.set_mode(Mode::EngineJournal).await?;
        match self.exec("COMMIT").await {
            Ok(()) => match self.is_autocommit().await {
                Ok(true) => Ok(()),
                Ok(false) => {
                    let unexpected = SqliteActorError::Poisoned(format!(
                        "{context} COMMIT returned success but the connection is still in a transaction"
                    ));
                    Err(self.cleanup_after_error(context, unexpected).await)
                }
                Err(probe) => Err(SqliteActorError::Poisoned(format!(
                    "could not confirm autocommit after {context} COMMIT: {probe}"
                ))),
            },
            Err(commit_error) => Err(self.cleanup_after_error(context, commit_error).await),
        }
    }

    /// Roll back a failed engine-owned transaction and return the original error
    /// only after proving that the connection is back in autocommit mode.
    pub(crate) async fn cleanup_after_error(
        &self,
        context: &str,
        original: SqliteActorError,
    ) -> SqliteActorError {
        if let Err(mode_error) = self.set_mode(Mode::EngineJournal).await {
            return SqliteActorError::Poisoned(format!(
                "could not enter engine mode to clean up {context}: {mode_error}; original error: {original}"
            ));
        }
        let rollback = self.exec("ROLLBACK").await;
        match self.is_autocommit().await {
            Ok(true) => original,
            Ok(false) => SqliteActorError::Poisoned(format!(
                "transaction still open after {context} ROLLBACK (rollback result: {rollback:?}); original error: {original}"
            )),
            Err(probe) => SqliteActorError::Poisoned(format!(
                "could not confirm autocommit after {context} ROLLBACK: {probe}; original error: {original}"
            )),
        }
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
/// CreatorUp ↔ EngineJournal without re-installing the closure.
struct HardenedConn {
    conn: Connection,
    mode: AuthMode,
    denials: DenialLog,
    journal_attached: bool,
}

impl HardenedConn {
    fn flip_mode(&self, mode: Mode) {
        self.mode.store(mode);
    }

    /// Open the statement-scoped denial window. EVERY statement runs inside one.
    fn denial_scope(&self) -> DenialScope<'_> {
        DenialScope::open(&self.denials)
    }
}

/// The statement-scoped view of the connection's last-denial slot.
///
/// The slot is emptied when the scope opens AND again when it drops, so a denial
/// recorded by one command can never be read by the next, including on an early
/// return, an error path, or a panic unwinding out of a statement. An RAII guard
/// rather than paired manual stores: there is no path that can forget the clear.
///
/// Cross-command observation is impossible for a second reason too: the actor is a
/// single OS thread draining one queue, so exactly one scope exists at a time and
/// no other command can read the slot while a statement holds it.
struct DenialScope<'a> {
    log: &'a DenialLog,
}

impl<'a> DenialScope<'a> {
    fn open(log: &'a DenialLog) -> Self {
        log.clear();
        DenialScope { log }
    }

    /// Map a rusqlite failure to [`SqliteActorError::Exec`], naming the refused
    /// action when the failure is an authorizer DENY.
    fn exec(&self, error: &rusqlite::Error) -> SqliteActorError {
        self.exec_message(error.to_string())
    }

    /// A non-authorizer failure is returned VERBATIM: the denial is appended only
    /// when [`SqliteActorError::is_authorizer_denied`] holds AND a denial was
    /// actually recorded for this statement, so the message can never claim a
    /// denial that did not happen.
    fn exec_message(&self, message: String) -> SqliteActorError {
        let error = SqliteActorError::Exec(message);
        if !error.is_authorizer_denied() {
            return error;
        }
        let Some(denial) = self.log.last() else {
            return error;
        };
        match error {
            SqliteActorError::Exec(message) => {
                SqliteActorError::Exec(format!("{message} [denied: {denial}]"))
            }
            other => other,
        }
    }
}

impl Drop for DenialScope<'_> {
    fn drop(&mut self) {
        self.log.clear();
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

/// Open + harden the connection per (exact order). Returns the wrapper that
/// keeps the [`AuthMode`] alive alongside the connection.
#[derive(Clone, Copy)]
enum JournalAttachment {
    IfPresent,
    Required,
}

fn open_hardened(
    app_path: &Path,
    journal_path: &Path,
    journal_attachment: JournalAttachment,
) -> Result<HardenedConn, SqliteActorError> {
    // Version floor — refuse to run below the supported SQLite.
    let v = rusqlite::version_number();
    if v < SQLITE_VERSION_FLOOR {
        return Err(SqliteActorError::UnsupportedVersion {
            found: v,
            floor: SQLITE_VERSION_FLOOR,
        });
    }

    // 0. Open the APP FILE as the connection's MAIN database, then ATTACH ONLY the
    // journal file as `_mig`. The app file is NOT ATTACHed a second time:
    // attaching a file that is also the main DB would open the SAME file twice on
    // one connection, and `BEGIN IMMEDIATE` would deadlock the two handles against
    // each other on the file's RESERVED lock. With `main` = the app file, the only
    // databases are `main` (app) and `_mig`, each opened exactly once, so a
    // single-connection `BEGIN IMMEDIATE` takes their RESERVED locks cleanly.
    //
    // Because `main` is the app file, an UNqualified creator `CREATE TABLE
    // users(...)` lands in — and PERSISTS to — the app file. NOTE: the
    // SQLite migration `up` MUST be emitted UNqualified (no `"<app_id>".table`
    // schema prefix) so it targets `main`. The shared emitter currently qualifies
    // as `"<app_id>".table` (a PG-schema shape); wiring the SQLite emitter to emit
    // unqualified DDL is the emitter-alias reconciliation. The authorizer
    // treats `main` (and the bare/None database) as the creator-writable target.
    let conn = Connection::open(app_path)
        .map_err(|e| SqliteActorError::Open(format!("open main (app file): {e}")))?;

    let journal_attached = match journal_attachment {
        JournalAttachment::Required => true,
        JournalAttachment::IfPresent => journal_path.try_exists().map_err(|error| {
            SqliteActorError::Open(format!(
                "inspect journal path {}: {error}",
                journal_path.display()
            ))
        })?,
    };
    if journal_attached {
        // ATTACH the journal AS `_mig` BEFORE the authorizer. On a fresh backend
        // the initial open skips this statement, because SQLite ATTACH would create
        // an absent file. Journal bootstrap reopens through `Required` here.
        conn.execute(
            &format!("ATTACH DATABASE ?1 AS \"{MIG_ALIAS}\""),
            [path_str(journal_path)?],
        )
        .map_err(|e| SqliteActorError::Open(format!("attach _mig: {e}")))?;
    }

    // 1. Engine-set PRAGMAs, applied at open BEFORE the authorizer (which denies
    // PRAGMA for the connection's life). `busy_timeout` gives a bounded wait so a
    // transient internal lock (e.g. between the RETURNING read and the next write
    // on the same connection across `main` + `_mig`) does not surface as an
    // immediate `SQLITE_BUSY`. `foreign_keys=ON` enables FK enforcement (a
    // per-connection setting SQLite ships OFF).
    conn.execute_batch("PRAGMA busy_timeout = 5000; PRAGMA foreign_keys = ON;")
        .map_err(|e| SqliteActorError::Open(format!("pragma bootstrap: {e}")))?;

    // A transaction that touches `main` and the attached `_mig` journal is
    // crash-atomic only when SQLite can use its super-journal protocol. WAL,
    // MEMORY, and OFF journal modes do not provide that guarantee across attached
    // databases. Pin both files to the conservative DELETE rollback-journal mode
    // and FULL synchronous, then read the effective values back. Never trust the
    // assignment alone: SQLite returns the mode it actually achieved and can
    // silently retain an old mode when a change is unavailable.
    enforce_atomic_profile(&conn, journal_attached)?;

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
    // the most-restrictive CreatorUp; the engine opts into EngineJournal only
    // for its own bootstrap + journal writes.
    let mode = AuthMode::new();
    let denials = DenialLog::new();
    conn.authorizer(Some(make_authorizer(mode.clone(), denials.clone())))
        .map_err(|e| SqliteActorError::Open(format!("install authorizer: {e}")))?;

    Ok(HardenedConn {
        conn,
        mode,
        denials,
        journal_attached,
    })
}

/// Pin and verify the settings required for crash-atomic commits spanning
/// `main` and the attached `_mig` journal.
#[cfg(test)]
fn enforce_atomic_attached_profile(conn: &Connection) -> Result<(), SqliteActorError> {
    enforce_atomic_profile(conn, true)
}

fn enforce_atomic_profile(
    conn: &Connection,
    journal_attached: bool,
) -> Result<(), SqliteActorError> {
    enforce_atomic_profile_for_schema(conn, "main")?;
    if journal_attached {
        enforce_atomic_profile_for_schema(conn, MIG_ALIAS)?;
    }
    Ok(())
}

fn enforce_atomic_profile_for_schema(
    conn: &Connection,
    schema: &str,
) -> Result<(), SqliteActorError> {
    let sql = format!("PRAGMA \"{schema}\".journal_mode = DELETE");
    let actual: String = conn.query_row(&sql, [], |row| row.get(0)).map_err(|e| {
        SqliteActorError::Open(format!(
            "set {schema}.journal_mode=DELETE for attached-DB atomicity: {e}"
        ))
    })?;
    if !actual.eq_ignore_ascii_case("delete") {
        return Err(SqliteActorError::Open(format!(
                "{schema}.journal_mode remained {actual:?}; DELETE rollback journaling is required for atomic app+journal commits"
            )));
    }

    conn.execute_batch(&format!("PRAGMA \"{schema}\".synchronous = FULL"))
        .map_err(|e| SqliteActorError::Open(format!("set {schema}.synchronous=FULL: {e}")))?;
    let synchronous: i64 = conn
        .query_row(&format!("PRAGMA \"{schema}\".synchronous"), [], |row| {
            row.get(0)
        })
        .map_err(|e| SqliteActorError::Open(format!("verify {schema}.synchronous: {e}")))?;
    // SQLite's stable numeric value for FULL is 2. EXTRA (3) is stronger but
    // not the pinned profile, so reject configuration drift in either direction.
    if synchronous != 2 {
        return Err(SqliteActorError::Open(format!(
                "{schema}.synchronous remained {synchronous}; FULL (2) is required for atomic app+journal commits"
            )));
    }
    Ok(())
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
fn run_exec(conn: &HardenedConn, sql: &str) -> Result<(), SqliteActorError> {
    let denials = conn.denial_scope();
    conn.execute_batch(sql).map_err(|e| denials.exec(&e))
}

/// Run ONE parameterized statement, binding `params` natively to its
/// `?n` placeholders (rusqlite `execute` with positional params). Prepared under
/// the current authorizer mode (a DENY surfaces as [`SqliteActorError::Exec`]).
/// MUST be a single statement (the mode is read at prepare).
fn run_exec_params(
    conn: &HardenedConn,
    sql: &str,
    params: &[SqliteBind],
) -> Result<(), SqliteActorError> {
    let denials = conn.denial_scope();
    let values: Vec<rusqlite::types::Value> = params.iter().map(SqliteBind::to_sql_value).collect();
    let refs: Vec<&dyn rusqlite::ToSql> =
        values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    conn.execute(sql, refs.as_slice())
        .map(|_| ())
        .map_err(|e| denials.exec(&e))
}

/// Run ONE parameterized statement that returns rows, binding `params`
/// natively to its `?n` placeholders, stringifying every cell for transport.
/// Prepared under the current authorizer mode (a DENY surfaces as
/// [`SqliteActorError::Exec`]). MUST be a single statement.
fn run_query_params(
    conn: &HardenedConn,
    sql: &str,
    params: &[SqliteBind],
) -> Result<Vec<Vec<Option<String>>>, SqliteActorError> {
    let denials = conn.denial_scope();
    let values: Vec<rusqlite::types::Value> = params.iter().map(SqliteBind::to_sql_value).collect();
    let refs: Vec<&dyn rusqlite::ToSql> =
        values.iter().map(|v| v as &dyn rusqlite::ToSql).collect();
    let mut stmt = conn.prepare(sql).map_err(|e| denials.exec(&e))?;
    let col_count = stmt.column_count();
    let rows = stmt
        .query_map(refs.as_slice(), |row| {
            let mut out = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let cell: Option<String> = match row.get_ref(i)? {
                    rusqlite::types::ValueRef::Null => None,
                    rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                    rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                    rusqlite::types::ValueRef::Text(t) => Some(
                        std::str::from_utf8(t)
                            .map_err(|e| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    i,
                                    rusqlite::types::Type::Text,
                                    Box::new(e),
                                )
                            })?
                            .to_owned(),
                    ),
                    rusqlite::types::ValueRef::Blob(b) => Some(hex::encode(b)),
                };
                out.push(cell);
            }
            Ok(out)
        })
        .map_err(|e| denials.exec(&e))?;
    let mut materialized = Vec::new();
    for r in rows {
        materialized.push(r.map_err(|e| denials.exec(&e))?);
    }
    Ok(materialized)
}

fn run_validate_text_utf8(conn: &HardenedConn, sql: &str) -> Result<(), SqliteActorError> {
    let denials = conn.denial_scope();
    let mut stmt = conn.prepare(sql).map_err(|error| denials.exec(&error))?;
    if stmt.column_count() != 1 {
        return Err(SqliteActorError::Exec(
            "UTF-8 cursor preflight must select exactly one column".to_string(),
        ));
    }
    let mut rows = stmt.query([]).map_err(|error| denials.exec(&error))?;
    let mut row_number = 0_u64;
    while let Some(row) = rows.next().map_err(|error| denials.exec(&error))? {
        row_number += 1;
        match row.get_ref(0).map_err(|error| denials.exec(&error))? {
            rusqlite::types::ValueRef::Text(value) => {
                if let Err(error) = std::str::from_utf8(value) {
                    return Err(SqliteActorError::Exec(format!(
                        "SQLite TEXT backfill cursor contains invalid UTF-8 at row {row_number}: {error}"
                    )));
                }
            }
            rusqlite::types::ValueRef::Null => {
                return Err(SqliteActorError::Exec(format!(
                    "SQLite TEXT backfill cursor contains NULL at row {row_number}"
                )));
            }
            other => {
                return Err(SqliteActorError::Exec(format!(
                    "SQLite TEXT backfill cursor has non-text storage class {:?} at row {row_number}",
                    other.data_type()
                )));
            }
        }
    }
    Ok(())
}

/// Run one query, stringifying every cell so the reply crosses the actor boundary.
fn run_query(conn: &HardenedConn, sql: &str) -> Result<Vec<Vec<Option<String>>>, SqliteActorError> {
    let denials = conn.denial_scope();
    let mut stmt = conn.prepare(sql).map_err(|e| denials.exec(&e))?;
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
                    rusqlite::types::ValueRef::Text(t) => Some(
                        std::str::from_utf8(t)
                            .map_err(|e| {
                                rusqlite::Error::FromSqlConversionFailure(
                                    i,
                                    rusqlite::types::Type::Text,
                                    Box::new(e),
                                )
                            })?
                            .to_owned(),
                    ),
                    rusqlite::types::ValueRef::Blob(b) => Some(hex::encode(b)),
                };
                out.push(cell);
            }
            Ok(out)
        })
        .map_err(|e| denials.exec(&e))?;
    let mut materialized = Vec::new();
    for r in rows {
        materialized.push(r.map_err(|e| denials.exec(&e))?);
    }
    Ok(materialized)
}

#[cfg(test)]
mod bind_tests {
    use super::*;

    #[test]
    fn sqlite_transaction_paths_use_commit_cleanup_helper() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/apply/backend/sqlite");
        for file in [
            "backfill_sql.rs",
            "journal_sql.rs",
            "rebuild_sql.rs",
            "rollback_sql.rs",
        ] {
            let source = std::fs::read_to_string(root.join(file)).expect("read SQLite source");
            assert!(
                !source.contains(".exec(\"COMMIT\")"),
                "{file} must route COMMIT failures through commit_or_cleanup"
            );
        }
    }

    #[test]
    fn binary_plan_bind_becomes_a_blob_value() {
        let bytes = vec![0, 1, 0x80, 0xff];
        let bind = SqliteBind::from_bind(&crate::render::step::BindValue::Bytes(bytes.clone()));
        assert!(matches!(
            bind.to_sql_value(),
            rusqlite::types::Value::Blob(value) if value == bytes
        ));
    }

    #[test]
    fn hardened_open_pins_both_files_to_crash_atomic_settings() {
        let dir = tempfile::tempdir().expect("tempdir");
        let conn = Connection::open(dir.path().join("app.sqlite")).expect("open main");
        conn.execute(
            &format!("ATTACH DATABASE ?1 AS \"{MIG_ALIAS}\""),
            [dir.path().join("journal.sqlite").to_string_lossy()],
        )
        .expect("attach journal");
        enforce_atomic_attached_profile(&conn).expect("pin atomic settings");

        for schema in ["main", MIG_ALIAS] {
            let mode: String = conn
                .query_row(&format!("PRAGMA \"{schema}\".journal_mode"), [], |row| {
                    row.get(0)
                })
                .expect("read journal mode");
            let synchronous: i64 = conn
                .query_row(&format!("PRAGMA \"{schema}\".synchronous"), [], |row| {
                    row.get(0)
                })
                .expect("read synchronous");
            assert_eq!(mode, "delete", "{schema} uses a rollback journal");
            assert_eq!(synchronous, 2, "{schema} uses FULL synchronous");
        }
    }

    #[compio::test]
    async fn failed_commit_rolls_back_and_leaves_actor_reusable() {
        let dir = tempfile::tempdir().expect("tempdir");
        let actor = MigrationActor::open(
            &dir.path().join("app.sqlite"),
            &dir.path().join("journal.sqlite"),
        )
        .expect("open actor");

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY); \
                 CREATE TABLE child (\
                    id INTEGER PRIMARY KEY, \
                    parent_id INTEGER NOT NULL, \
                    FOREIGN KEY (parent_id) REFERENCES parent(id) \
                        DEFERRABLE INITIALLY DEFERRED\
                 )",
            )
            .await
            .expect("create deferred foreign key");

        actor
            .set_mode(Mode::EngineJournal)
            .await
            .expect("engine mode");
        actor.exec("BEGIN IMMEDIATE").await.expect("begin");
        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        actor
            .exec("INSERT INTO child (id, parent_id) VALUES (1, 99)")
            .await
            .expect("deferred violation is accepted before commit");

        let error = actor
            .commit_or_cleanup("test transaction")
            .await
            .expect_err("deferred foreign key makes COMMIT fail");
        assert!(matches!(error, SqliteActorError::Exec(_)), "{error:?}");
        assert!(actor.is_autocommit().await.expect("autocommit probe"));

        actor.set_mode(Mode::CreatorUp).await.expect("creator mode");
        let rows = actor
            .query("SELECT count(*) FROM child")
            .await
            .expect("actor remains reusable");
        assert_eq!(rows[0][0].as_deref(), Some("0"));
    }
}
