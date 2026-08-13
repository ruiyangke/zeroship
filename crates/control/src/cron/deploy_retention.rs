//! Durable-workflow deploy-bundle retention sweep.
//!
//! A workflow run pins the deploy it started on via `workflow_runs.deploy_id`.
//! The authoritative refcount is therefore derived from each app's workflow
//! journal on demand; no platform refcount table is maintained.

use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use compio_postgres::GenericClient;
use uuid::Uuid;

use zeroship_core::config::DeclaredEnvKey;

use crate::config::ControlSettingsConsumer;
use crate::registry::RegistryError;
use crate::AppState;

pub const DEFAULT_TICK_SECS: u64 = 60 * 60;
pub const DEFAULT_BATCH_SIZE: i64 = 128;
pub const DEFAULT_GRACE_WINDOW_MS: i64 = 0;
pub const GRACE_WINDOW_ENV: DeclaredEnvKey<String, ControlSettingsConsumer> =
    DeclaredEnvKey::external("CONTROL_DEPLOY_RETENTION_GRACE_WINDOW_MS");
pub const BATCH_SIZE_ENV: DeclaredEnvKey<String, ControlSettingsConsumer> =
    DeclaredEnvKey::external("CONTROL_DEPLOY_RETENTION_BATCH_SIZE");

const SWEEP_LOCK: &str = "zeroship.deploy_retention";

#[derive(Debug, Clone, Copy)]
pub struct DeployRetentionConfig {
    pub grace_window_ms: i64,
    pub batch_size: i64,
}

impl Default for DeployRetentionConfig {
    fn default() -> Self {
        let grace_window_raw = zeroship_core::read_declared_env!(GRACE_WINDOW_ENV, ControlSettingsConsumer)
            .ok()
            .flatten();
        let batch_size_raw = zeroship_core::read_declared_env!(BATCH_SIZE_ENV, ControlSettingsConsumer)
            .ok()
            .flatten();
        Self {
            grace_window_ms: nonnegative_env_i64(GRACE_WINDOW_ENV.name(), grace_window_raw)
                .unwrap_or(DEFAULT_GRACE_WINDOW_MS),
            batch_size: positive_env_i64(BATCH_SIZE_ENV.name(), batch_size_raw)
                .unwrap_or(DEFAULT_BATCH_SIZE),
        }
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeployRetentionStats {
    pub candidates: usize,
    pub retained_live_pins: usize,
    pub manifests_deleted: usize,
}

impl DeployRetentionStats {
    fn is_empty(self) -> bool {
        self.candidates == 0 && self.retained_live_pins == 0 && self.manifests_deleted == 0
    }
}

#[derive(Debug)]
struct DeployCandidate {
    app_id: Uuid,
    deploy_id: String,
    deploy_hash: String,
}

#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    let config = DeployRetentionConfig::default();
    tracing::info!(
        tick_secs,
        grace_window_ms = config.grace_window_ms,
        batch_size = config.batch_size,
        "control deploy_retention cron starting"
    );
    loop {
        match tick_with_config(&state, config).await {
            Ok(stats) if !stats.is_empty() => tracing::info!(
                candidates = stats.candidates,
                retained_live_pins = stats.retained_live_pins,
                manifests_deleted = stats.manifests_deleted,
                "deploy_retention tick processed deploy manifests"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "deploy_retention tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<DeployRetentionStats, RegistryError> {
    tick_with_config(state, DeployRetentionConfig::default()).await
}

#[allow(clippy::future_not_send)]
pub async fn tick_with_config(
    state: &AppState,
    config: DeployRetentionConfig,
) -> Result<DeployRetentionStats, RegistryError> {
    let grace_window_ms = config.grace_window_ms.max(0);
    let mut remaining = config.batch_size.max(1);
    let cutoff = Utc::now() - chrono::Duration::milliseconds(grace_window_ms);

    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await?;
    if !try_xact_lock(&tx, SWEEP_LOCK).await? {
        tx.commit().await?;
        return Ok(DeployRetentionStats::default());
    }

    let mut stats = DeployRetentionStats::default();
    for app_id in super::workflow_engine::workflow_app_ids(&tx).await? {
        if remaining <= 0 {
            break;
        }
        let candidates = superseded_deploys(&tx, &app_id, &cutoff, remaining).await?;
        for candidate in candidates {
            stats.candidates += 1;
            let pinned = deploy_pinned_run_count(&tx, &candidate.app_id, &candidate.deploy_id).await?;
            if pinned > 0 {
                stats.retained_live_pins += 1;
                continue;
            }
            if reclaim_manifest_if_guarded(state, &tx, &candidate, pinned).await? {
                stats.manifests_deleted += 1;
            }
            remaining -= 1;
            if remaining <= 0 {
                break;
            }
        }
    }

    tx.commit().await?;
    Ok(stats)
}

/// Count non-terminal workflow runs pinned to `deploy_id` for `app_id`.
///
/// This is the durable-workflow deploy refcount. It is derived from the per-app
/// journal, so terminal commits are the drain event and no worker-side release
/// message is required.
pub async fn deploy_pinned_run_count<C>(
    conn: &C,
    app_id: &Uuid,
    deploy_id: &str,
) -> Result<i64, RegistryError>
where
    C: GenericClient + Sync,
{
    let Some(tables) = super::workflow_engine::existing_tables(conn, app_id).await? else {
        return Ok(0);
    };
    let row = conn
        .query_one(
            &super::workflow_engine::journal_sql(
                &tables,
                "SELECT COUNT(*)::bigint AS n \
                   FROM zeroship.workflow_runs \
                  WHERE app_id = $1 \
                    AND deploy_id = $2 \
                    AND state NOT IN ('completed','failed','cancelled','stalled')",
            ),
            &[app_id, &deploy_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(row.get("n"))
}

/// Return true only when a deploy is superseded and has no live pinned runs.
pub async fn deploy_bundle_reclaimable<C>(
    conn: &C,
    app_id: &Uuid,
    deploy_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    if !deploy_is_superseded(conn, app_id, deploy_id).await? {
        return Ok(false);
    }
    Ok(deploy_pinned_run_count(conn, app_id, deploy_id).await? == 0)
}

async fn superseded_deploys<C>(
    conn: &C,
    app_id: &Uuid,
    cutoff: &DateTime<Utc>,
    limit: i64,
) -> Result<Vec<DeployCandidate>, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT d.id, d.deploy_hash \
               FROM zeroship.app_deploys d \
              WHERE d.app_id = $1 \
                AND d.activated_at IS NOT NULL \
                AND d.activated_at <= $2 \
                AND EXISTS ( \
                    SELECT 1 \
                      FROM zeroship.app_deploys newer \
                     WHERE newer.app_id = d.app_id \
                       AND newer.activated_at IS NOT NULL \
                       AND (newer.activated_at, newer.created_at, newer.id) \
                           > (d.activated_at, d.created_at, d.id) \
                ) \
              ORDER BY d.activated_at, d.created_at, d.id \
              LIMIT $3",
            &[app_id, cutoff, &limit],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .into_iter()
        .map(|row| DeployCandidate {
            app_id: *app_id,
            deploy_id: row.get("id"),
            deploy_hash: row.get("deploy_hash"),
        })
        .collect())
}

async fn deploy_is_superseded<C>(
    conn: &C,
    app_id: &Uuid,
    deploy_id: &str,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT EXISTS ( \
                    SELECT 1 \
                      FROM zeroship.app_deploys d \
                     WHERE d.id = $1 \
                       AND d.app_id = $2 \
                       AND d.activated_at IS NOT NULL \
                       AND EXISTS ( \
                           SELECT 1 \
                             FROM zeroship.app_deploys newer \
                            WHERE newer.app_id = d.app_id \
                              AND newer.activated_at IS NOT NULL \
                              AND (newer.activated_at, newer.created_at, newer.id) \
                                  > (d.activated_at, d.created_at, d.id) \
                       ) \
                ) AS superseded",
            &[&deploy_id, app_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows.first().is_some_and(|row| row.get("superseded")))
}

async fn reclaim_manifest_if_guarded<C>(
    state: &AppState,
    conn: &C,
    candidate: &DeployCandidate,
    pinned_count: i64,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT deploy_hash \
               FROM zeroship.app_deploys \
              WHERE id = $1 AND app_id = $2 \
              FOR UPDATE SKIP LOCKED",
            &[&candidate.deploy_id, &candidate.app_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Ok(false);
    };
    let deploy_hash: String = row.get("deploy_hash");
    if deploy_hash != candidate.deploy_hash {
        return Ok(false);
    }
    if pinned_count != 0 || !deploy_bundle_reclaimable(conn, &candidate.app_id, &candidate.deploy_id).await? {
        return Ok(false);
    }

    match state
        .blob_store
        .delete_manifest(&candidate.app_id, &candidate.deploy_hash)
        .await
    {
        Ok(deleted) => Ok(deleted),
        Err(e) => {
            tracing::warn!(
                app_id = %candidate.app_id,
                deploy_id = %candidate.deploy_id,
                deploy_hash = %candidate.deploy_hash,
                error = %e,
                "deploy_retention manifest delete failed"
            );
            Ok(false)
        }
    }
}

async fn try_xact_lock<C>(conn: &C, key: &str) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT pg_try_advisory_xact_lock(hashtextextended($1::text, 0::bigint)) AS locked",
            &[&key],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows.first().is_some_and(|row| row.get("locked")))
}

fn positive_env_i64(name: &str, raw: Option<String>) -> Option<i64> {
    let raw = raw?;
    match raw.parse::<i64>() {
        Ok(value) if value > 0 => Some(value),
        Ok(_) | Err(_) => {
            tracing::warn!(env = name, value = %raw, "ignoring non-positive integer env");
            None
        }
    }
}

fn nonnegative_env_i64(name: &str, raw: Option<String>) -> Option<i64> {
    let raw = raw?;
    match raw.parse::<i64>() {
        Ok(value) if value >= 0 => Some(value),
        Ok(_) | Err(_) => {
            tracing::warn!(env = name, value = %raw, "ignoring negative integer env");
            None
        }
    }
}
