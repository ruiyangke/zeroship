//! `SqliteSession` actor — stub.
//!
//! **P1 PR 1**: type names + `Command` enum variants exist so
//! [`super::SqliteBackend::session`] can be typed and the eventual
//! `SqlExecutor::Client` associated type wired. Every method body is
//! `todo!()` or `unimplemented!()` with a "PR 2 stub" sentinel.
//!
//! **P1 PR 2** lands the real implementation: a `compio::runtime::spawn_blocking`
//! actor owning a single `rusqlite::Connection`, fed via a bounded
//! `flume` mpsc queue; one writer per [`super::SqliteBackend`]
//! instance (design §8, §18 Q8). The session bootstraps PRAGMAs
//! (`journal_mode=WAL`, `synchronous=NORMAL`, `busy_timeout=5000`,
//! `foreign_keys=ON`) on `open` and runs each `Command` synchronously
//! inside the worker thread.
//!
//! **Cancel safety** (PR 2): every queued command carries a
//! `flume::oneshot` reply; cancelling the awaiting future drops the
//! receiver but the worker still completes the SQL (WAL-durable or
//! rolled back). Next `rx.recv()` picks up the next command.

use std::path::Path;
use std::rc::Rc;

use crate::error::DbError;

/// Commands queued onto the [`SqliteSession`] actor.
///
/// **P1 PR 1 stub**: variants exist so PR 2 can wire the actor body
/// without re-shuffling the enum at the call sites. Parameters here
/// will gain the actual SQL / param / `flume::Sender<Result<_, _>>`
/// reply shape in PR 2.
#[allow(dead_code)]
pub(crate) enum Command {
    /// Run a non-row-returning statement and reply with the affected
    /// row count.
    Exec,
    /// Run a row-returning statement and reply with the materialised
    /// rows.
    Query,
    /// `ATTACH DATABASE '...' AS '<app_id>'` — namespace-bootstrap
    /// dispatch surface for the `NamespaceManager` impl (PR 3).
    Attach,
    /// `DETACH DATABASE '<app_id>'` — paired with `Attach` for
    /// completeness; PR 1 has no consumer.
    Detach,
    /// Drain the queue and shut the worker thread down. Sent by
    /// [`SqliteSession`]'s `Drop` impl.
    Shutdown,
}

/// Single-writer actor wrapping a `rusqlite::Connection`.
///
/// **P1 PR 1 stub**: the field set is the eventual shape but every
/// field is `()` until PR 2 wires the actor. The struct exists so
/// [`super::SqliteBackend::session`] can be typed at PR 1 and the
/// trait impls can name `Rc<SqliteSession>` in their bodies (as
/// `todo!()`s). `pub` because `SqliteSessionHandle` (the
/// `SqlExecutor::Client` associated type) wraps it in `Rc<…>`.
#[allow(dead_code)]
pub struct SqliteSession {
    /// Placeholder for the `flume::Sender<Command>` channel head.
    _tx: (),
    /// Placeholder for the `compio::runtime::Task<()>` worker handle.
    _worker: (),
}

impl SqliteSession {
    /// Open a SQLite database at `db_path` and spawn the writer actor.
    ///
    /// **P1 PR 1 stub**: returns a typed `DbError::Internal` with a
    /// sentinel so attempted construction surfaces a clear failure
    /// mode in tests / runtime ahead of PR 2.
    #[allow(dead_code)]
    pub(crate) fn open(_db_path: &Path) -> Result<Self, DbError> {
        Err(DbError::Internal {
            message: "SqliteSession::open — P1 PR2 stub".into(),
        })
    }
}

/// Clone-cheap handle to a [`SqliteSession`]. Wraps `Rc<SqliteSession>`.
///
/// **P1 PR 1 stub**: exists so `SqlExecutor::Client` on
/// [`super::SqliteBackend`] can be wired at PR 1. `pub` because the
/// `SqlExecutor` trait this surfaces as is itself `pub` — the type
/// must be reachable from any consumer that names `<B as SqlExecutor>::Client`
/// on a SQLite-arm backend. Fields stay private.
#[derive(Clone)]
pub struct SqliteSessionHandle {
    _inner: Rc<SqliteSession>,
}

impl SqliteSessionHandle {
    /// Wrap an existing session in a handle.
    #[allow(dead_code)]
    pub(crate) fn new(session: Rc<SqliteSession>) -> Self {
        Self { _inner: session }
    }
}
