//! Durable-workflow output blob GC.
//!
//! This sweeps only the workflow `wfblob/` namespace. Deploy bundle blobs and
//! manifests are managed by their own storage contract and are never listed or
//! deleted here.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use chrono::Utc;
use compio_postgres::GenericClient;
use zeroship_plugin_workflow::store::pg::WorkflowTables;

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

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct BlobGcStats {
    pub deleted_refs: usize,
    pub deleted_orphans: usize,
}

#[allow(clippy::future_not_send)]
pub async fn run_ref_sweep(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_blob_ref_gc cron starting");
    loop {
        match tick_ref_sweep(&state).await {
            Ok(n) if n > 0 => tracing::info!(deleted = n, "workflow_blob_ref_gc tick deleted blobs"),
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

#[allow(clippy::future_not_send)]
pub async fn tick_ref_sweep(state: &AppState) -> Result<usize, RegistryError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await?;
    if !try_xact_lock(&tx, REF_SWEEP_LOCK).await? {
        tx.commit().await?;
        return Ok(0);
    }

    let cutoff = Utc::now() - chrono::Duration::seconds(REF_SWEEP_GRACE_SECS);
    let mut deleted = 0usize;
    let mut remaining = MAX_REF_DELETES_PER_TICK;
    for app_id in super::workflow_engine::workflow_app_ids(&tx).await? {
        if remaining <= 0 {
            break;
        }
        let Some(tables) = super::workflow_engine::existing_tables(&tx, &app_id).await? else {
            continue;
        };
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

        for row in rows {
            let hash: String = row.get("hash");
            if delete_zero_ref_blob_locked_for_app(state, &tx, &tables, &hash).await? {
                deleted += 1;
                remaining -= 1;
                if remaining <= 0 {
                    break;
                }
            }
        }
    }

    tx.commit().await?;
    Ok(deleted)
}

pub(crate) async fn delete_zero_ref_hashes_for_app(
    state: &AppState,
    tables: &WorkflowTables,
    hashes: impl IntoIterator<Item = String>,
) -> Result<usize, RegistryError> {
    let hashes: BTreeSet<String> = hashes.into_iter().collect();
    if hashes.is_empty() {
        return Ok(0);
    }

    let mut conn = state.registry.conn().await?;
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
        if workflow_blob_is_referenced(&tx, &entry.hash).await? {
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

async fn workflow_blob_is_referenced<C>(conn: &C, hash: &str) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    for app_id in super::workflow_engine::workflow_app_ids(conn).await? {
        let Some(tables) = super::workflow_engine::existing_tables(conn, &app_id).await? else {
            continue;
        };
        let rows = conn
            .query(
                &super::workflow_engine::journal_sql(
                    &tables,
                    "SELECT \
                        EXISTS (SELECT 1 FROM zeroship.workflow_blobs WHERE hash = $1) \
                     OR EXISTS (SELECT 1 FROM zeroship.workflow_steps \
                                 WHERE output_kind = 'blob' AND output_hash = $1) \
                     OR EXISTS (SELECT 1 FROM zeroship.workflow_runs \
                                 WHERE output_kind = 'blob' AND output_hash = $1) AS referenced",
                ),
                &[&hash],
            )
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

    let delete_sql = format!(
        "DELETE FROM {} WHERE hash = $1 AND refcount = 0",
        tables.blobs
    );
    let changed = conn.execute(&delete_sql, &[&hash]).await?;
    if changed == 0 || workflow_blob_is_referenced(conn, hash).await? {
        return Ok(false);
    }

    match state.workflow_blob_store.delete_blob(hash).await {
        Ok(()) => Ok(true),
        Err(e) => {
            tracing::warn!(hash = %hash, error = %e, "workflow blob GC delete failed");
            Ok(false)
        }
    }
}
