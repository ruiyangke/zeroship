//! Status + history read API (design §2.2, scenarios 45/46) — **read-only**.
//!
//! [`status`] answers "where is this project's schema?" — what's applied, what's
//! pending (in the exact order apply will run it), the current version, and what
//! has been rolled back. [`history`] returns the FULL append-only audit log
//! (every apply + every rollback event, in order), the tamper-evident record of
//! every state transition the journal ever saw.
//!
//! This module emits NO DDL and mutates nothing — it surfaces journal state. It
//! reuses the journal's NET-state reader ([`journal::applied`]) and the
//! executor's pending-ordering ([`crate::executor::order_pending`]) so status's
//! view of "applied" and "pending" is byte-for-byte the view apply itself uses.

use std::collections::HashMap;

use compio_postgres::Client;

use crate::db::ExecutorConfig;
use crate::executor::{order_pending, ApplyError};
use crate::journal::{self, AppliedEntry, HistoryEvent, JournalError, Phase, RolledBackEntry};
use crate::migration::{Migration, MigrationId};

/// Where a project's schema stands relative to a supplied migration set.
///
/// `applied` and `pending` are computed from **NET journal state** (a rolled-back
/// version is NOT applied and re-enters `pending`); `rolled_back` lists versions
/// whose latest event is a rollback. The three are derived from the same journal
/// read the executor uses, so status never disagrees with what apply would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationStatus {
    /// The highest net-applied version (the schema's current point), or `None`
    /// when nothing is applied. "Highest" is `UUIDv7`/`MigrationId` order — the
    /// same total order apply advances through.
    pub current_version: Option<MigrationId>,
    /// Net-applied entries (latest event = `completed`), in version order. Reuses
    /// [`journal::applied`]'s entries (version, checksum, phase).
    pub applied: Vec<AppliedEntry>,
    /// Versions in the supplied set that are NOT net-applied, in the SAME
    /// topological order apply will run them ([`crate::executor::order_pending`]).
    /// A rolled-back version that is still in the set reappears here.
    pub pending: Vec<MigrationId>,
    /// Versions whose latest event is a rollback (net rolled-back), with the
    /// rollback event's detail.
    pub rolled_back: Vec<RolledBackEntry>,
}

/// Error from the status/history read API.
#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    /// A journal read failed.
    #[error(transparent)]
    Journal(#[from] JournalError),
    /// Computing the pending order failed (an unsatisfiable `depends_on` or a
    /// dependency cycle in the supplied set) — surfaced, not swallowed, so status
    /// reports the same ordering fault apply would hit.
    #[error("pending ordering: {0}")]
    Ordering(#[source] ApplyError),
}

/// Compute the [`MigrationStatus`] of `migrations` against the journal — what is
/// applied, pending, current, and rolled back (design scenarios 45/46).
///
/// **Read-only.** Bootstraps the journal idempotently (so a fresh project reports
/// cleanly), then derives every field from NET journal state. `applied` reuses
/// [`journal::applied`]; `pending` reuses the executor's [`order_pending`] (same
/// topo order as apply); `current_version` is the highest net-applied version;
/// `rolled_back` is from [`journal::net_rolled_back`].
///
/// **Consistent snapshot.** The two journal reads (`applied` and
/// `net_rolled_back`) run inside ONE `REPEATABLE READ READ ONLY` transaction, so a
/// concurrent apply/rollback committing between them can never split the view into
/// an inconsistent applied-vs-rolled-back bucketing. The transaction is driven
/// explicitly over the shared `&Client` (`BEGIN … COMMIT`), mirroring how the
/// executor drives its apply/rollback transactions. `ensure_journal` (which emits
/// `CREATE … IF NOT EXISTS` DDL) runs BEFORE the snapshot, since a `READ ONLY`
/// transaction forbids DDL and bootstrap must stay idempotent regardless.
///
/// "Current" = highest-VERSION net-applied (`UUIDv7`/`MigrationId` total order),
/// NOT most-recently-applied. The two coincide unless a `depends_on` graph drove
/// apply order away from version order.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection. This function takes whatever
/// [`Client`] it is handed and never elevates to the `migrator` role; schema
/// scoping by `cfg.meta_schema` keeps reads bound to this project's journal, but
/// the privilege of the connection is the caller's obligation.
///
/// # Errors
/// - [`StatusError::Journal`] on a journal read/bootstrap failure.
/// - [`StatusError::Ordering`] if the supplied set's `depends_on` is
///   unsatisfiable or cyclic (the same fault apply would surface).
pub async fn status(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    journal::ensure_journal(conn, cfg).await?;

    // One consistent snapshot over both journal reads (applied + rolled_back). A
    // REPEATABLE READ READ ONLY txn pins a single MVCC view, so a concurrent
    // commit between the two reads can't produce a split bucket view.
    conn.batch_execute("BEGIN ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .await
        .map_err(|e| StatusError::Journal(JournalError::Db(e)))?;
    let snapshot = read_status_snapshot(conn, cfg, migrations).await;
    // Always close the txn; a read-only snapshot has nothing to roll back, but we
    // must not leak an open transaction onto the shared session.
    let _ = conn.batch_execute("COMMIT").await;
    snapshot
}

/// Backend-generic [`status`]: compute the [`MigrationStatus`] over ANY
/// [`MigrationBackend`](crate::backend::MigrationBackend), reading net journal
/// state through the trait (`ensure_journal` + `applied` + `superseded_versions`)
/// rather than a PG `&Client`. This is the multi-engine peer of [`status`] — the
/// public CLI's SQLite leg routes here, where the PG leg keeps the
/// `REPEATABLE READ READ ONLY` snapshot path above (the SQLite actor serializes
/// structurally, so a single net-state read is already a consistent view).
///
/// `applied` / `pending` / `current_version` are derived with the SAME rules and
/// the SAME [`order_pending`] the executor uses, so status never disagrees with
/// what apply would do. `rolled_back` is left empty for backends that expose no
/// net-rolled-back reader on the neutral trait (SQLite): a rolled-back version is
/// already absent from `applied` net-state, so it correctly re-enters `pending`.
///
/// # Errors
/// - [`StatusError::Journal`] on a journal bootstrap/read failure.
/// - [`StatusError::Ordering`] if the set's `depends_on` is unsatisfiable/cyclic
///   (the same fault apply would surface).
pub async fn status_via_backend<B: crate::backend::MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    backend.ensure_journal(cfg).await?;

    let entries = backend.applied(cfg).await?;
    let applied: Vec<AppliedEntry> = entries
        .iter()
        .filter(|e| e.phase == Phase::Completed)
        .cloned()
        .collect();

    let current_version = applied
        .iter()
        .filter_map(|e| MigrationId::parse(&e.version).ok())
        .max();

    let completed: HashMap<&str, &AppliedEntry> =
        applied.iter().map(|e| (e.version.as_str(), e)).collect();
    let journal_superseded = backend.superseded_versions(cfg).await?;
    let superseded_owned = crate::executor::compute_superseded(migrations, &journal_superseded);
    let superseded: std::collections::HashSet<&str> =
        superseded_owned.iter().map(String::as_str).collect();
    let ordered =
        order_pending(migrations, &completed, &superseded).map_err(StatusError::Ordering)?;
    let pending: Vec<MigrationId> = ordered.iter().map(|m| m.version.clone()).collect();

    Ok(MigrationStatus {
        current_version,
        applied,
        pending,
        // The neutral trait exposes no net-rolled-back reader; a rolled-back
        // version is already dropped from `applied` net-state (it reappears in
        // `pending`), so an empty list here is honest, not lossy.
        rolled_back: Vec::new(),
    })
}

/// The body of [`status`]'s consistent-snapshot read: both journal reads + the
/// derived fields, run inside the caller's open `REPEATABLE READ READ ONLY` txn.
async fn read_status_snapshot(
    conn: &Client,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> Result<MigrationStatus, StatusError> {
    let entries = journal::applied(conn, cfg).await?;
    // NET-applied entries only (drop lone `started` inflight markers — those are
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

    // pending = set − net-applied − superseded, in the SAME order apply uses.
    // order_pending wants a map of completed entries keyed by version; build it from
    // the net-applied entries (NOT the raw rows — a rolled-back version must count
    // as pending, and net state already excludes it).
    let completed: HashMap<&str, &AppliedEntry> =
        applied.iter().map(|e| (e.version.as_str(), e)).collect();
    // Supersession (Plan 9 squash): a version superseded by a net-applied squash OR
    // by an in-set squash is NOT pending — status must agree with apply. Reuses the
    // executor's `compute_superseded` so the two views never diverge.
    let journal_superseded = journal::superseded_versions(conn, cfg).await?;
    let superseded_owned =
        crate::executor::compute_superseded(migrations, &journal_superseded);
    let superseded: std::collections::HashSet<&str> =
        superseded_owned.iter().map(String::as_str).collect();
    let ordered =
        order_pending(migrations, &completed, &superseded).map_err(StatusError::Ordering)?;
    let pending: Vec<MigrationId> = ordered.iter().map(|m| m.version.clone()).collect();

    let rolled_back = journal::net_rolled_back(conn, cfg).await?;

    Ok(MigrationStatus {
        current_version,
        applied,
        pending,
        rolled_back,
    })
}

/// Read the FULL append-only event log (every apply + rollback event) in
/// `event_seq` order — the audit trail (design §2.2, scenario 46).
///
/// **Read-only.** Unlike [`status`], this does NOT collapse to net state: a
/// version applied → rolled back → re-applied shows all three events. Bootstraps
/// the journal idempotently first so a fresh project returns an empty log.
///
/// # Preconditions
/// The caller MUST pass an **admin/read** connection. Like [`status`] and
/// [`snapshot_schema`](crate::snapshot_schema), this takes whatever [`Client`] it
/// is handed and never elevates to the `migrator` role; the reads are scoped to
/// `cfg.meta_schema`, but the connection's privilege is the caller's obligation.
///
/// # Errors
/// [`StatusError::Journal`] on a journal read/bootstrap failure.
pub async fn history(
    conn: &Client,
    cfg: &ExecutorConfig,
) -> Result<Vec<HistoryEvent>, StatusError> {
    journal::ensure_journal(conn, cfg).await?;
    Ok(journal::history(conn, cfg).await?)
}
