//! Durable-workflow output blob GC.
//!
//! This sweeps only the workflow `wfblob/` namespace. Deploy bundle blobs and
//! manifests are managed by their own storage contract and are never listed or
//! deleted here.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use compio_postgres::{Client, GenericClient};
use uuid::Uuid;
use zeroship_workflow::store::pg::WorkflowTables;

use crate::cron::workflow_engine::SweepCoverage;
use crate::registry::RegistryError;
use crate::AppState;

// G3/to-measure — operator-pending
pub const DEFAULT_REF_SWEEP_TICK_SECS: u64 = 15 * 60;
// G3/to-measure — operator-pending
pub const DEFAULT_ORPHAN_SWEEP_TICK_SECS: u64 = 30 * 60;
// G3/to-measure — operator-pending
pub const REF_SWEEP_GRACE_SECS: i64 = 24 * 60 * 60;
// G3/to-measure — operator-pending
pub const ORPHAN_SWEEP_GRACE_SECS: i64 = 72 * 60 * 60;

const REF_SWEEP_LOCK: &str = "zeroship.workflow_blob_ref_gc";
const ORPHAN_SWEEP_LOCK: &str = "zeroship.workflow_blob_orphan_gc";
const MAX_REF_DELETES_PER_TICK: i64 = 256;

/// What one [`tick_ref_sweep`] did, and over how much of the fleet.
///
/// An app excluded from this sweep keeps its zero-refcount blobs, so the cost
/// of a silent exclusion is unbounded storage growth on that tenant - which no
/// delete count can report, because the healthy answer is also a small number.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RefSweepStats {
    /// Blobs deleted from the object store and their app's blob table.
    pub deleted: usize,
    pub coverage: SweepCoverage,
}

#[allow(clippy::future_not_send)]
pub async fn run_ref_sweep(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_blob_ref_gc cron starting");
    loop {
        match tick_ref_sweep(&state).await {
            Ok(stats) if stats.deleted > 0 || stats.coverage.apps_skipped > 0 => tracing::info!(
                deleted = stats.deleted,
                apps_swept = stats.coverage.apps_swept,
                apps_skipped = stats.coverage.apps_skipped,
                apps_unvisited = stats.coverage.apps_unvisited,
                "workflow_blob_ref_gc tick deleted blobs"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_blob_ref_gc tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

#[allow(clippy::future_not_send)]
pub async fn run_orphan_sweep(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_blob_orphan_gc cron starting");
    loop {
        match tick_orphan_sweep(&state).await {
            Ok(n) if n > 0 => tracing::info!(deleted = n, "workflow_blob_orphan_gc tick deleted blobs"),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_blob_orphan_gc tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

/// Delete each app's own zero-refcount workflow blobs.
///
/// ONE transaction for the whole tick, which decides what an exclusion can be
/// here. A journal that the catalog reported readable and that then DENIES the
/// per-app statement cannot be skipped: the failed statement has already
/// aborted this transaction, so there is nothing to carry on with and the error
/// propagates. Only the catalog-level exclusion lands in `apps_skipped`. That
/// is the trade this sweep's transaction scope buys, and it is why
/// `apps_skipped` here is not the same population as in the per-app-transaction
/// sweeps.
#[allow(clippy::future_not_send)]
pub async fn tick_ref_sweep(state: &AppState) -> Result<RefSweepStats, RegistryError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await?;
    if !try_xact_lock(&tx, REF_SWEEP_LOCK).await? {
        tx.commit().await?;
        // Another node holds the sweep lock. Zero coverage, not complete
        // coverage: this tick swept nothing and must not claim it did.
        return Ok(RefSweepStats::default());
    }

    let cutoff = Utc::now() - chrono::Duration::seconds(REF_SWEEP_GRACE_SECS);
    let mut stats = RefSweepStats::default();
    let mut remaining = MAX_REF_DELETES_PER_TICK;
    let fleet = super::workflow_engine::journalled_fleet(&tx).await?;
    stats.coverage = SweepCoverage::opened_over(&fleet);
    let mut apps = fleet.usable.into_iter();
    // Walked by `next()` rather than by `for`, and the budget is checked BEFORE
    // the pull, so that a `break` leaves every untouched app IN the iterator
    // for `len()` below. A `for` loop - or a check after the pull - would have
    // already moved the breaking app out and undercounted by one.
    loop {
        if remaining <= 0 {
            break;
        }
        let Some(app_id) = apps.next() else {
            break;
        };
        // Deletes only this app's own zero-refcount blobs, so an app excluded
        // for being unreadable costs that app its GC and nothing else. The
        // ORPHAN sweep below is the opposite case and must not skip.
        let tables = WorkflowTables::for_app_id(&app_id);
        let rows = tx
            .query(
                &super::workflow_engine::journal_sql(
                    &tables,
            "SELECT b.hash \
               FROM zeroship.workflow_blobs b \
              WHERE b.refcount = 0 \
                AND b.last_referenced_at <= $1 \
                AND NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_steps s \
                     WHERE s.output_kind = 'blob' AND s.output_hash = b.hash \
                ) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_runs r \
                     WHERE r.output_kind = 'blob' AND r.output_hash = b.hash \
                ) \
              ORDER BY b.last_referenced_at, b.hash \
              LIMIT $2 \
              FOR UPDATE SKIP LOCKED",
                ),
                &[&cutoff, &remaining],
            )
            .await?;
        stats.coverage.swept_one();

        for row in rows {
            let hash: String = row.get("hash");
            if delete_zero_ref_blob_locked_for_app(state, &tx, &tables, &hash).await? {
                stats.deleted += 1;
                remaining -= 1;
                if remaining <= 0 {
                    break;
                }
            }
        }
    }
    stats.coverage.apps_unvisited = apps.len();

    tx.commit().await?;
    Ok(stats)
}

/// Delete one app's now-unreferenced blobs, in a transaction of its own on the
/// CALLER's connection.
///
/// The connection is borrowed rather than opened here so that a fleet sweep
/// pays one connect for the whole tick instead of one per app; the transaction
/// is still per call, so the caller's own per-app boundary is preserved.
pub(crate) async fn delete_zero_ref_hashes_for_app(
    state: &AppState,
    conn: &mut Client,
    tables: &WorkflowTables,
    hashes: impl IntoIterator<Item = String>,
) -> Result<usize, RegistryError> {
    let hashes: BTreeSet<String> = hashes.into_iter().collect();
    if hashes.is_empty() {
        return Ok(0);
    }

    let tx = conn.transaction().await?;
    let mut deleted = 0usize;
    for hash in hashes {
        if delete_zero_ref_blob_locked_for_app(state, &tx, tables, &hash).await? {
            deleted += 1;
        }
    }
    tx.commit().await?;
    Ok(deleted)
}

#[allow(clippy::future_not_send)]
pub async fn tick_orphan_sweep(state: &AppState) -> Result<usize, RegistryError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await?;
    if !try_xact_lock(&tx, ORPHAN_SWEEP_LOCK).await? {
        tx.commit().await?;
        return Ok(0);
    }

    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(ORPHAN_SWEEP_GRACE_SECS as u64))
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let entries = state
        .workflow_blob_store
        .list_blobs()
        .await
        .map_err(|e| RegistryError::Database(e.to_string()))?;

    let mut deleted = 0usize;
    for entry in entries {
        if entry.last_modified > cutoff {
            continue;
        }
        // No `owner`: this sweep deletes an OBJECT and no row, so every blob
        // row in the fleet - including one in the app that wrote this object -
        // is a live reference that must retain it.
        if workflow_blob_is_referenced(&tx, &entry.hash, None).await? {
            continue;
        }
        match state.workflow_blob_store.delete_blob(&entry.hash).await {
            Ok(()) => deleted += 1,
            Err(e) => {
                tracing::warn!(
                    hash = %entry.hash,
                    size = entry.size,
                    error = %e,
                    "workflow blob orphan GC delete failed"
                );
            }
        }
    }

    tx.commit().await?;
    Ok(deleted)
}

async fn try_xact_lock<C>(conn: &C, key: &str) -> Result<bool, RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1::text, 0::bigint)) AS locked",
            &[&key],
        )
        .await?;
    Ok(rows.first().is_some_and(|row| row.get("locked")))
}

/// Every way a hash can still be in use: a blob row, a step output, a run
/// output.
const BLOB_REFERENCED_SQL: &str = "SELECT \
        EXISTS (SELECT 1 FROM zeroship.workflow_blobs WHERE hash = $1) \
     OR EXISTS (SELECT 1 FROM zeroship.workflow_steps \
                 WHERE output_kind = 'blob' AND output_hash = $1) \
     OR EXISTS (SELECT 1 FROM zeroship.workflow_runs \
                 WHERE output_kind = 'blob' AND output_hash = $1) AS referenced";

/// The same question, minus the blob row itself.
///
/// Used for the ONE app whose own blob row is the row about to be deleted: that
/// row is what the caller is collecting, so counting it would make the answer
/// "referenced" every time. Its steps and runs are still searched, which its
/// only caller has ALREADY established with the `NOT EXISTS` arms of its
/// `FOR UPDATE` select. Keeping them is deliberate and untestable from outside:
/// it makes this function answer its own question rather than one that is only
/// true because of a guard in the caller.
const BLOB_REFERENCED_BY_OUTPUT_SQL: &str = "SELECT \
        EXISTS (SELECT 1 FROM zeroship.workflow_steps \
                 WHERE output_kind = 'blob' AND output_hash = $1) \
     OR EXISTS (SELECT 1 FROM zeroship.workflow_runs \
                 WHERE output_kind = 'blob' AND output_hash = $1) AS referenced";

/// Is `hash` referenced by ANY app's journal?
///
/// The caller DELETES the blob when this says false, so an app whose journal
/// cannot be SEARCHED must answer TRUE, not be skipped. Skipping it would let a
/// permission gap on one tenant delete another tenant's live workflow output -
/// a fleet-wide sweep that reads "I could not check" as "nothing there" turns a
/// visible outage into silent data loss.
///
/// "Cannot be searched" covers an incomplete journal as well as an unreadable
/// one, and the incomplete case is why the census reports it at all: the
/// reference query reads `blobs`, `steps` AND `runs`, so a journal missing one
/// of them cannot answer the question - and an app the census DROPPED would
/// have been passed over silently, letting a hash its blobs table still
/// references be deleted.
///
/// `owner` names the app whose own blob row is the row the caller is about to
/// delete, and excludes THAT ROW ONLY from the search - the same app's steps and
/// runs are still searched, and every other app is searched in full. Passing
/// `None` searches everything, which is what the orphan sweep wants: it deletes
/// no row, so no row is its own.
async fn workflow_blob_is_referenced<C>(
    conn: &C,
    hash: &str,
    owner: Option<&Uuid>,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    for app in super::workflow_engine::journalled_apps(conn).await? {
        let app_id = app.app_id;
        if let Some(reason) = app.exclusion() {
            tracing::warn!(
                app_id = %app_id,
                hash = %hash,
                "retaining workflow blob: {reason}, so it cannot be proven unreferenced"
            );
            return Ok(true);
        }
        let tables = WorkflowTables::for_app_id(&app_id);
        let sql = if owner == Some(&app_id) {
            BLOB_REFERENCED_BY_OUTPUT_SQL
        } else {
            BLOB_REFERENCED_SQL
        };
        let rows = conn
            .query(&super::workflow_engine::journal_sql(&tables, sql), &[&hash])
            .await?;
        if rows.first().is_some_and(|row| row.get("referenced")) {
            return Ok(true);
        }
    }
    Ok(false)
}

async fn delete_zero_ref_blob_locked_for_app<C>(
    state: &AppState,
    conn: &C,
    tables: &WorkflowTables,
    hash: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            &super::workflow_engine::journal_sql(
                tables,
            "SELECT b.hash \
               FROM zeroship.workflow_blobs b \
              WHERE b.hash = $1 \
                AND b.refcount = 0 \
                AND NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_steps s \
                     WHERE s.output_kind = 'blob' AND s.output_hash = b.hash \
                ) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_runs r \
                     WHERE r.output_kind = 'blob' AND r.output_hash = b.hash \
                ) \
              FOR UPDATE SKIP LOCKED",
            ),
            &[&hash],
        )
        .await?;
    if rows.is_empty() {
        return Ok(false);
    }

    // Prove it unreferenced BEFORE removing the row, not after.
    //
    // This check ran after the DELETE until 2026-08-20, and the ordering was
    // buying one thing: `workflow_blob_is_referenced` counts a blob ROW as a
    // reference, so with this app's own row still present it answered
    // "referenced" every time and nothing would ever have been collected.
    // Deleting first made the row invisible to the check. `owner` buys the same
    // exclusion without the destructive step, and buys it more narrowly - only
    // that one row is excluded, where a DELETE excluded it by making it gone.
    //
    // What the old ordering COST is why this moved. A `false` verdict arrived
    // with the row already deleted, and `Ok(false)` does not roll back: the
    // enclosing transaction in `tick_ref_sweep` and in
    // `delete_zero_ref_hashes_for_app` commits either way. One unreadable
    // tenant makes the check say "referenced" for EVERY hash in the fleet, so
    // the row went and the object stayed - storage neither sweep can reclaim,
    // because the ref sweep walks rows and finds none while the orphan sweep
    // refuses on the same unreadable journal.
    if workflow_blob_is_referenced(conn, hash, Some(&tables.app_id)).await? {
        return Ok(false);
    }

    let delete_sql = format!(
        "DELETE FROM {} WHERE hash = $1 AND refcount = 0",
        tables.blobs
    );
    // `changed == 0` is still honoured as "someone else got there first", but
    // the race guard is the `FOR UPDATE SKIP LOCKED` above, not this count: a
    // second sweep is skipped past the locked row and returns above, and THIS
    // app taking a fresh reference is an `INSERT ... ON CONFLICT DO UPDATE SET
    // refcount = refcount + 1` on the very row we hold (zeroship-workflow
    // src/store/pg.rs), so it blocks until we commit.
    //
    // What that lock does NOT cover, and did not cover before this reordering
    // either: a DIFFERENT app taking its first reference to the same hash
    // writes ITS OWN blobs table and is not blocked, so it can appear between
    // the check above and the `delete_blob` below and be left with a row whose
    // object is gone. Moving the check earlier widens that window by one
    // statement; it does not create it. The orphan sweep is not the backstop
    // for it either - it deletes objects, not rows.
    let changed = conn.execute(&delete_sql, &[&hash]).await?;
    if changed == 0 {
        return Ok(false);
    }

    // The object goes AFTER the row, and this arm still commits the row delete
    // with the object present. That asymmetry is deliberate and is not the
    // defect above: the other order would delete bytes a rolled-back
    // transaction still has a row for, and here the ORPHAN sweep is the
    // reclaimer, which it can be precisely because the row is gone. It only
    // reclaims while the fleet's journals are readable - so a failed store
    // delete during a permission gap leaks until that gap closes.
    match state.workflow_blob_store.delete_blob(hash).await {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::warn!(hash = %hash, error = %e, "workflow blob GC delete failed");
            Ok(false)
        }
    }
}
