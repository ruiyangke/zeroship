//! Durable-workflow terminal-run retention sweep.
//!
//! This sweep prunes only terminal workflow runs whose `terminal_at` is older
//! than the operator-tunable retention window. It is intentionally separate
//! from the workflow claim loop: claim scheduling can be paused without turning
//! off journal retention.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use compio_postgres::GenericClient;
use zeroship_plugin_workflow::store::pg::WorkflowTables;

use zeroship_core::config::DeclaredEnvKey;

use crate::config::ControlSettingsConsumer;
use crate::cron::workflow_blob_gc;
use crate::registry::RegistryError;
use crate::AppState;

pub const DEFAULT_TICK_SECS: u64 = 60 * 60;
pub const DEFAULT_BATCH_SIZE: i64 = 128;
pub const DEFAULT_RETENTION_WINDOW_MS: i64 = 7 * 24 * 60 * 60 * 1_000;
pub const RETENTION_WINDOW_ENV: DeclaredEnvKey<String, ControlSettingsConsumer> =
    DeclaredEnvKey::platform("CONTROL_WORKFLOW_RETENTION_WINDOW_MS");
pub const BATCH_SIZE_ENV: DeclaredEnvKey<String, ControlSettingsConsumer> =
    DeclaredEnvKey::platform("CONTROL_WORKFLOW_RETENTION_BATCH_SIZE");

#[derive(Debug, Clone, Copy)]
pub struct WorkflowRetentionConfig {
    pub retention_window_ms: i64,
    pub batch_size: i64,
}

impl Default for WorkflowRetentionConfig {
    fn default() -> Self {
        let retention_window_raw =
            zeroship_core::read_declared_env!(RETENTION_WINDOW_ENV, ControlSettingsConsumer)
                .ok()
                .flatten();
        let batch_size_raw =
            zeroship_core::read_declared_env!(BATCH_SIZE_ENV, ControlSettingsConsumer)
                .ok()
                .flatten();
        Self {
            retention_window_ms: positive_env_i64(retention_window_raw)
                .unwrap_or(DEFAULT_RETENTION_WINDOW_MS),
            batch_size: positive_env_i64(batch_size_raw).unwrap_or(DEFAULT_BATCH_SIZE),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct RetentionStats {
    pub runs: usize,
    pub steps: usize,
    pub signals: usize,
    pub subscriptions: usize,
    pub broadcasts: usize,
    pub blobs: usize,
}

impl RetentionStats {
    fn add(&mut self, other: Self) {
        self.runs += other.runs;
        self.steps += other.steps;
        self.signals += other.signals;
        self.subscriptions += other.subscriptions;
        self.broadcasts += other.broadcasts;
        self.blobs += other.blobs;
    }

    fn is_empty(self) -> bool {
        self.runs == 0
            && self.steps == 0
            && self.signals == 0
            && self.subscriptions == 0
            && self.broadcasts == 0
            && self.blobs == 0
    }
}

#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    let config = WorkflowRetentionConfig::default();
    tracing::info!(
        tick_secs,
        retention_window_ms = config.retention_window_ms,
        batch_size = config.batch_size,
        "control workflow_retention cron starting"
    );
    loop {
        match tick_with_config(&state, config).await {
            Ok(stats) if !stats.is_empty() => tracing::info!(
                runs = stats.runs,
                steps = stats.steps,
                signals = stats.signals,
                subscriptions = stats.subscriptions,
                broadcasts = stats.broadcasts,
                blobs = stats.blobs,
                "workflow_retention tick reaped rows"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_retention tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<RetentionStats, RegistryError> {
    tick_with_config(state, WorkflowRetentionConfig::default()).await
}

#[allow(clippy::future_not_send)]
pub async fn tick_with_config(
    state: &AppState,
    config: WorkflowRetentionConfig,
) -> Result<RetentionStats, RegistryError> {
    let retention_window_ms = config.retention_window_ms.max(1);
    let mut remaining_batch = config.batch_size.max(1);
    let cutoff = Utc::now() - chrono::Duration::milliseconds(retention_window_ms);

    let app_ids = {
        let conn = state.registry.conn().await?;
        super::workflow_engine::journalled_app_ids(&conn).await?
    };
    let mut stats = RetentionStats::default();
    for app_id in app_ids {
        if remaining_batch <= 0 {
            break;
        }
        let mut conn = state.registry.conn().await?;
        let tx = conn.transaction().await.map_err(RegistryError::from)?;
        // No per-app existence probe: `journalled_app_ids` already returned only
        // apps whose journal exists AND is readable on this connection, so the
        // `to_regclass` round trip that used to run here per app is redundant.
        let tables = WorkflowTables::for_app_id(&app_id);
        let sql = super::workflow_engine::journal_sql(
            &tables,
            "SELECT r.id \
               FROM zeroship.workflow_runs r \
              WHERE r.state IN ('completed','failed','cancelled','stalled') \
                AND r.terminal_at IS NOT NULL \
                AND r.terminal_at <= $1 \
                AND NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_runs child \
                     WHERE child.parent_run_id = r.id \
                       AND NOT ( \
                           child.state IN ('completed','failed','cancelled','stalled') \
                           AND child.terminal_at IS NOT NULL \
                           AND child.terminal_at <= $1 \
                       ) \
                ) \
              ORDER BY r.tree_depth DESC, r.terminal_at, r.id \
              LIMIT $2 \
              FOR UPDATE SKIP LOCKED",
        );
        let rows = tx
            .query(&sql, &[&cutoff, &remaining_batch])
            .await
            .map_err(RegistryError::from)?;

        let mut app_blob_hashes = BTreeSet::new();
        for row in rows {
            let run_id: String = row.get("id");
            tx.batch_execute("SAVEPOINT workflow_retention_run")
                .await
                .map_err(RegistryError::from)?;
            match prune_one_run(&tx, &tables, &run_id, &cutoff).await {
                Ok(Some(pruned)) => {
                    tx.batch_execute("RELEASE SAVEPOINT workflow_retention_run")
                        .await
                        .map_err(RegistryError::from)?;
                    remaining_batch = remaining_batch.saturating_sub(1);
                    stats.add(pruned.stats);
                    app_blob_hashes.extend(pruned.blob_hashes);
                }
                Ok(None) => {
                    tx.batch_execute("RELEASE SAVEPOINT workflow_retention_run")
                        .await
                        .map_err(RegistryError::from)?;
                }
                Err(e) => {
                    tracing::warn!(run_id = %run_id, error = %e, "workflow_retention row prune failed");
                    tx.batch_execute("ROLLBACK TO SAVEPOINT workflow_retention_run")
                        .await
                        .map_err(RegistryError::from)?;
                    tx.batch_execute("RELEASE SAVEPOINT workflow_retention_run")
                        .await
                        .map_err(RegistryError::from)?;
                }
            }
        }
        tx.commit().await.map_err(RegistryError::from)?;
        stats.blobs = workflow_blob_gc::delete_zero_ref_hashes_for_app(
            state,
            &tables,
            app_blob_hashes,
        )
        .await?
        .saturating_add(stats.blobs);
    }
    Ok(stats)
}

struct PrunedRun {
    stats: RetentionStats,
    blob_hashes: Vec<String>,
}

async fn prune_one_run<C>(
    tx: &C,
    tables: &WorkflowTables,
    run_id: &str,
    cutoff: &DateTime<Utc>,
) -> Result<Option<PrunedRun>, RegistryError>
where
    C: GenericClient + Sync,
{
    let counts = tx
        .query_one(
            &super::workflow_engine::journal_sql(
                tables,
            "SELECT \
                (SELECT COUNT(*)::bigint FROM zeroship.workflow_steps WHERE run_id = $1) AS steps, \
                (SELECT COUNT(*)::bigint FROM zeroship.workflow_signals WHERE run_id = $1) AS signals, \
                (SELECT COUNT(*)::bigint FROM zeroship.workflow_subscriptions WHERE run_id = $1) AS subscriptions",
            ),
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;

    let blob_refs = tx
        .query(
            &super::workflow_engine::journal_sql(
                tables,
            "SELECT hash, SUM(refs)::bigint AS refs \
               FROM ( \
                    SELECT output_hash AS hash, COUNT(*)::bigint AS refs \
                      FROM zeroship.workflow_steps \
                     WHERE run_id = $1 \
                       AND output_kind = 'blob' \
                       AND output_hash IS NOT NULL \
                     GROUP BY output_hash \
                    UNION ALL \
                    SELECT output_hash AS hash, COUNT(*)::bigint AS refs \
                      FROM zeroship.workflow_runs \
                     WHERE id = $1 \
                       AND output_kind = 'blob' \
                       AND output_hash IS NOT NULL \
                     GROUP BY output_hash \
               ) refs \
              GROUP BY hash \
              ORDER BY hash",
            ),
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let blob_hashes: Vec<String> = blob_refs.iter().map(|row| row.get("hash")).collect();
    let blob_counts: Vec<i64> = blob_refs.iter().map(|row| row.get("refs")).collect();

    let broadcast_rows = tx
        .query(
            &super::workflow_engine::journal_sql(
                tables,
            "SELECT DISTINCT broadcast_id \
               FROM zeroship.workflow_signals \
              WHERE run_id = $1 AND broadcast_id IS NOT NULL \
              ORDER BY broadcast_id",
            ),
            &[&run_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let broadcast_ids: Vec<String> = broadcast_rows
        .iter()
        .map(|row| row.get("broadcast_id"))
        .collect();

    let deleted = tx
        .execute(
            &super::workflow_engine::journal_sql(
                tables,
            "DELETE FROM zeroship.workflow_runs r \
              WHERE r.id = $1 \
                AND r.state IN ('completed','failed','cancelled','stalled') \
                AND r.terminal_at IS NOT NULL \
                AND r.terminal_at <= $2 \
                AND NOT EXISTS ( \
                        SELECT 1 FROM zeroship.workflow_runs child \
                     WHERE child.parent_run_id = r.id \
                )",
            ),
            &[&run_id, cutoff],
        )
        .await
        .map_err(RegistryError::from)?;
    if deleted == 0 {
        return Ok(None);
    }

    if !blob_hashes.is_empty() {
        tx.execute(
            &super::workflow_engine::journal_sql(
                tables,
            "UPDATE zeroship.workflow_blobs b \
                SET refcount = GREATEST(b.refcount::bigint - refs.refs, 0)::int, \
                    last_referenced_at = now() \
               FROM ( \
                    SELECT * FROM unnest($1::text[], $2::bigint[]) AS r(hash, refs) \
               ) refs \
              WHERE b.hash::text = refs.hash",
            ),
            &[&blob_hashes, &blob_counts],
        )
        .await
        .map_err(RegistryError::from)?;
    }

    let mut broadcasts = 0usize;
    if !broadcast_ids.is_empty() {
        broadcasts = tx
            .execute(
                &format!(
                "DELETE FROM zeroship.workflow_broadcasts b \
                  WHERE b.id = ANY($1) \
                    AND b.app_id = $2 \
                    AND b.expires_at <= now() \
                    AND NOT EXISTS ( \
                        SELECT 1 FROM {} sig \
                         WHERE sig.broadcast_id = b.id \
                    )",
                    tables.signals,
                ),
                &[&broadcast_ids, &tables.app_id],
            )
            .await
            .map_err(RegistryError::from)? as usize;
    }

    Ok(Some(PrunedRun {
        stats: RetentionStats {
            runs: 1,
            steps: nonnegative_count(&counts, "steps"),
            signals: nonnegative_count(&counts, "signals"),
            subscriptions: nonnegative_count(&counts, "subscriptions"),
            broadcasts,
            blobs: 0,
        },
        blob_hashes,
    }))
}

fn nonnegative_count(row: &compio_postgres::Row, column: &str) -> usize {
    row.get::<_, i64>(column).max(0) as usize
}

fn positive_env_i64(raw: Option<String>) -> Option<i64> {
    raw?.trim().parse::<i64>().ok().filter(|v| *v > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_retention_window_is_named_and_conservative() {
        assert_eq!(DEFAULT_RETENTION_WINDOW_MS, 7 * 24 * 60 * 60 * 1_000);
        const { assert!(DEFAULT_RETENTION_WINDOW_MS > 0) };
    }
}
