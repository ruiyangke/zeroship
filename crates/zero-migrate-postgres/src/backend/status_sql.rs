//! The PostgreSQL status read: net journal state under a `REPEATABLE READ READ
//! ONLY` snapshot.
//!
//! This body used to live in `zero_migrate::ops::status` under the neutral name `status`,
//! and from there it reached `apply::backend::postgres::journal_sql` five times.
//! Core's status verb WAS PostgreSQL's status verb - nothing about the signature
//! (`&D: SqlSession`, a `dialect` argument) could have routed it anywhere else, and
//! the SQL it drives is PostgreSQL's: `BEGIN ISOLATION LEVEL REPEATABLE READ READ
//! ONLY` is not a statement MySQL or SQLite accepts.
//!
//! Its DIALECT-NEUTRAL peer is
//! `zero_migrate::ops::status::status_via_backend`, which reads the
//! same net state through [`MigrationBackend`](zero_migrate_backend::backend::MigrationBackend)
//! and is what the shipped CLI/addon path uses on every dialect including this one.
//! The two differ in ONE field: [`MigrationStatus::rolled_back`](zero_migrate_backend::status::MigrationStatus::rolled_back) is populated here
//! and left empty there, because the neutral trait exposes rollback VERSION IDS
//! (`net_rolled_back_versions`) while this path reads the full
//! [`RolledBackEntry`](zero_migrate_backend::journal::RolledBackEntry) detail. MySQL and
//! SQLite have no `net_rolled_back` returning that detail, so the two signatures do
//! not unify and the field was not forced onto the contract.

use std::collections::HashMap;

use zero_migrate_backend::conn::ExecutorConfig;
use zero_migrate_backend::driver::SqlSession;
use zero_migrate_backend::executor::order_pending;
use zero_migrate_backend::journal::{AppliedEntry, JournalError, Phase};
use zero_migrate_backend::status::{derive_pending_contract_status, MigrationStatus, StatusError};
use zero_migrate_ir::migration::{Migration, MigrationId};

use super::journal_sql;

/// Compute the [`MigrationStatus`] of `migrations` against the journal - what is
/// applied, pending, current, and rolled back (design scenarios 45/46).
///
/// **Read-only.** Bootstraps the journal idempotently (so a fresh project reports
/// cleanly), then derives every field from NET journal state. `applied` reuses
/// [`journal_sql::applied`]; `pending` reuses the executor's `order_pending` (same
/// topo order as apply); `current_version` is the highest net-applied version;
/// `rolled_back` is from [`journal_sql::net_rolled_back`].
///
/// **Consistent snapshot.** The two journal reads (`applied` and
/// `net_rolled_back`) run inside ONE `REPEATABLE READ READ ONLY` transaction, so a
/// concurrent apply/rollback committing between them can never split the view into
/// an inconsistent applied-vs-rolled-back bucketing. The transaction is driven
/// explicitly through the shared [`SqlSession`], mirroring how the
/// executor drives its apply/rollback transactions. `ensure_journal` (which emits
/// `CREATE ... IF NOT EXISTS` DDL) runs BEFORE the snapshot, since a `READ ONLY`
/// transaction forbids DDL and bootstrap must stay idempotent regardless.
///
/// "Current" = highest-VERSION net-applied (`UUIDv7`/`MigrationId` total order),
/// NOT most-recently-applied. The two coincide unless a `depends_on` graph drove
/// apply order away from version order.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection. This function takes whatever
/// [`SqlSession`] implementation it is handed and never
/// elevates to the `migrator` role; schema
/// scoping by `cfg.meta_schema` keeps reads bound to this project's journal, but
/// the privilege of the connection is the caller's obligation.
///
/// # Errors
/// - [`StatusError::Journal`] on a journal read/bootstrap failure.
/// - [`StatusError::Ordering`] if the supplied set's `depends_on` is
///   unsatisfiable or cyclic (the same fault apply would surface).
pub async fn status<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    journal_sql::ensure_journal(conn, cfg).await?;

    // One consistent snapshot over both journal reads (applied + rolled_back). A
    // REPEATABLE READ READ ONLY txn pins a single MVCC view, so a concurrent
    // commit between the two reads can't produce a split bucket view.
    conn.batch("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|e| StatusError::Journal(JournalError::Db(e.into())))?;
    let snapshot = read_status_snapshot(conn, cfg, migrations).await;
    finish_status_snapshot(conn, snapshot).await
}

async fn finish_status_snapshot<D: SqlSession, T>(
    conn: &D,
    snapshot: Result<T, StatusError>,
) -> Result<T, StatusError> {
    match snapshot {
        Ok(status) => {
            if let Err(commit_error) = conn.batch("COMMIT").await {
                if let Err(rollback_error) = conn.batch("ROLLBACK").await {
                    tracing::warn!(
                        error = %rollback_error,
                        "zero-migrate: PostgreSQL status rollback after COMMIT failure failed"
                    );
                }
                return Err(StatusError::Journal(JournalError::Db(commit_error.into())));
            }
            Ok(status)
        }
        Err(snapshot_error) => {
            if let Err(rollback_error) = conn.batch("ROLLBACK").await {
                tracing::warn!(
                    error = %rollback_error,
                    "zero-migrate: PostgreSQL status snapshot rollback failed"
                );
            }
            Err(snapshot_error)
        }
    }
}

/// The body of [`status`]'s consistent-snapshot read: both journal reads + the
/// derived fields, run inside the caller's open `REPEATABLE READ READ ONLY` txn.
async fn read_status_snapshot<D: SqlSession>(
    conn: &D,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    let entries = journal_sql::applied(conn, cfg).await?;
    // NET-applied entries only (drop lone `started` inflight markers - those are
    // crash-recovery keys, not settled applied state).
    let applied: Vec<AppliedEntry> = entries
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .cloned()
        .collect();

    // current_version = highest net-applied version (MigrationId order).
    let current_version = applied
        .iter()
        .filter_map(|e| MigrationId::parse(&e.version).ok())
        .max();

    // pending = set - net-applied - superseded, in the SAME order apply uses.
    // order_pending wants a map of completed entries keyed by version; build it from
    // the net-applied entries (NOT the raw rows - a rolled-back version must count
    // as pending, and net state already excludes it).
    let completed: HashMap<&str, &AppliedEntry> =
        applied.iter().map(|e| (e.version.as_str(), e)).collect();
    // Supersession (squash): a version superseded by a net-applied squash OR
    // by an in-set squash is NOT pending - status must agree with apply. Reuses the
    // executor's `compute_superseded` so the two views never diverge.
    let journal_superseded = journal_sql::superseded_versions(conn, cfg).await?;
    let superseded_owned =
        zero_migrate_backend::executor::compute_superseded(migrations, &journal_superseded);
    let superseded: std::collections::HashSet<&str> =
        superseded_owned.iter().map(String::as_str).collect();
    let ordered =
        order_pending(migrations, &completed, &superseded).map_err(StatusError::Ordering)?;
    let pending: Vec<MigrationId> = ordered.iter().map(|m| m.version.clone()).collect();

    let rolled_back = journal_sql::net_rolled_back(conn, cfg).await?;

    // Surface the outstanding cross-deploy pending contracts
    // (with orphan detection) + the plans blocked on a pending-contract
    // dependency. Read inside this same REPEATABLE READ READ ONLY snapshot so the
    // obligation view is consistent with the applied/rolled-back buckets.
    let outstanding = journal_sql::outstanding_pending_contracts(conn, cfg).await?;
    let (pending_contracts, blocked) = derive_pending_contract_status(&outstanding, migrations);

    Ok(MigrationStatus {
        current_version,
        applied,
        pending,
        rolled_back,
        pending_contracts,
        blocked,
    })
}

#[cfg(test)]
mod legacy_snapshot_transaction_tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use zero_migrate_backend::driver::{Bind, DbError, Row};

    struct RecordingSession {
        batches: RefCell<Vec<String>>,
        fail_commit: Cell<bool>,
    }

    impl RecordingSession {
        fn new(fail_commit: bool) -> Self {
            Self {
                batches: RefCell::new(Vec::new()),
                fail_commit: Cell::new(fail_commit),
            }
        }
    }

    impl SqlSession for RecordingSession {
        async fn batch(&self, sql: &str) -> Result<(), DbError> {
            self.batches.borrow_mut().push(sql.to_string());
            if sql == "COMMIT" && self.fail_commit.get() {
                return Err(DbError::message("injected status COMMIT failure"));
            }
            Ok(())
        }

        async fn exec(&self, _sql: &str, _binds: &[Bind]) -> Result<u64, DbError> {
            Err(DbError::message("unexpected exec"))
        }

        async fn exec_text(&self, _sql: &str, _params: &[Option<String>]) -> Result<u64, DbError> {
            Err(DbError::message("unexpected exec_text"))
        }

        async fn query(&self, _sql: &str, _binds: &[Bind]) -> Result<Vec<Row>, DbError> {
            Err(DbError::message("unexpected query"))
        }

        async fn query_one(&self, _sql: &str, _binds: &[Bind]) -> Result<Row, DbError> {
            Err(DbError::message("unexpected query_one"))
        }
    }

    #[compio::test]
    async fn snapshot_error_rolls_back_instead_of_committing() {
        let conn = RecordingSession::new(false);
        let result: Result<(), StatusError> = finish_status_snapshot(
            &conn,
            Err(StatusError::PlanManifest(
                "injected snapshot failure".into(),
            )),
        )
        .await;

        assert!(matches!(result, Err(StatusError::PlanManifest(_))));
        assert_eq!(conn.batches.borrow().as_slice(), ["ROLLBACK"]);
    }

    #[compio::test]
    async fn commit_failure_is_surfaced_and_cleanup_is_attempted() {
        let conn = RecordingSession::new(true);
        let error = finish_status_snapshot(&conn, Ok::<_, StatusError>(()))
            .await
            .expect_err("COMMIT failure must not be swallowed");

        assert!(error.to_string().contains("injected status COMMIT failure"));
        assert_eq!(conn.batches.borrow().as_slice(), ["COMMIT", "ROLLBACK"]);
    }
}
