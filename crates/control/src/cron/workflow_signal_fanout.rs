//! Durable-workflow topic broadcast fan-out sweep.
//!
//! Broadcasts are durable rows. This sweep is a peer of the workflow claim
//! sweep: it claims pending broadcast rows with `SKIP LOCKED`, delivers normal
//! `workflow_signals` rows to matching subscriptions, and relies on the
//! `(broadcast_id, run_id)` marker to make re-drain idempotent after a crash.

use std::sync::Arc;

use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::typed_id;

use crate::registry::RegistryError;
use crate::AppState;

pub const DEFAULT_TICK_SECS: u64 = 1;
pub const DEFAULT_MAX_BROADCASTS_PER_TICK: i64 = 16;
pub const DEFAULT_MAX_DELIVERIES_PER_BROADCAST: i64 = 100;

#[derive(Debug, Clone, Copy)]
pub struct FanoutSweepConfig {
    pub max_broadcasts_per_tick: i64,
    pub max_deliveries_per_broadcast: i64,
}

impl Default for FanoutSweepConfig {
    fn default() -> Self {
        Self {
            max_broadcasts_per_tick: DEFAULT_MAX_BROADCASTS_PER_TICK,
            max_deliveries_per_broadcast: DEFAULT_MAX_DELIVERIES_PER_BROADCAST,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FanoutStats {
    pub broadcasts: i64,
    pub deliveries: i64,
}

impl FanoutStats {
    fn add(&mut self, other: Self) {
        self.broadcasts += other.broadcasts;
        self.deliveries += other.deliveries;
    }
}

#[allow(clippy::future_not_send)]
pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    tracing::info!(tick_secs, "control workflow_signal_fanout cron starting");
    loop {
        match tick_with_config(&state, FanoutSweepConfig::default()).await {
            Ok(stats) if stats.broadcasts > 0 || stats.deliveries > 0 => tracing::info!(
                broadcasts = stats.broadcasts,
                deliveries = stats.deliveries,
                "workflow_signal_fanout tick delivered broadcasts"
            ),
            Ok(_) => {}
            Err(e) => tracing::error!(error = %e, "workflow_signal_fanout tick failed"),
        }
        compio::time::sleep(std::time::Duration::from_secs(tick_secs)).await;
    }
}

#[allow(clippy::future_not_send)]
pub async fn tick(state: &AppState) -> Result<FanoutStats, RegistryError> {
    tick_with_config(state, FanoutSweepConfig::default()).await
}

#[allow(clippy::future_not_send)]
pub async fn tick_with_config(
    state: &AppState,
    config: FanoutSweepConfig,
) -> Result<FanoutStats, RegistryError> {
    let max_broadcasts = config.max_broadcasts_per_tick.max(1);
    let max_deliveries = config.max_deliveries_per_broadcast.max(1);
    let mut stats = FanoutStats::default();
    gc_expired_subscriptions(state).await?;
    for _ in 0..max_broadcasts {
        let drained = drain_one_broadcast(state, max_deliveries).await?;
        if drained.broadcasts == 0 {
            break;
        }
        stats.add(drained);
    }
    Ok(stats)
}

async fn gc_expired_subscriptions(state: &AppState) -> Result<(), RegistryError> {
    let conn = state.registry.conn().await?;
    conn.execute(
        "DELETE FROM zeroship.workflow_subscriptions \
          WHERE expires_at IS NOT NULL AND expires_at < now()",
        &[],
    )
    .await
    .map_err(RegistryError::from)?;
    Ok(())
}

async fn drain_one_broadcast(
    state: &AppState,
    max_deliveries: i64,
) -> Result<FanoutStats, RegistryError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let rows = tx
        .query(
            "WITH picked AS ( \
                 SELECT b.id \
                   FROM zeroship.workflow_broadcasts b \
                   JOIN zeroship.apps app ON app.id = b.app_id \
                   JOIN zeroship.plans plan ON plan.id = app.plan_id \
                  WHERE b.fanout_state = 'pending' \
                    AND app.workflows_enabled \
                    AND plan.workflows_allowed \
                    AND NOT plan.archived \
                  ORDER BY b.created_at, b.id \
                  LIMIT 1 \
                  FOR UPDATE SKIP LOCKED \
             ) \
             SELECT b.id, b.app_id, b.topic, b.type, b.payload, b.origin, b.provider, \
                    b.idempotency_key, b.expires_at \
               FROM zeroship.workflow_broadcasts b \
               JOIN picked p ON p.id = b.id",
            &[],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(FanoutStats::default());
    };

    let broadcast_id: String = row.get("id");
    let app_id: Uuid = row.get("app_id");
    let topic: String = row.get("topic");
    let signal_type: String = row.get("type");
    let payload: Value = row.get("payload");
    let origin: String = row.get("origin");
    let provider: Option<String> = row.get("provider");
    let idempotency_key: String = row.get("idempotency_key");
    let expires_at: DateTime<Utc> = row.get("expires_at");

    if expires_at <= Utc::now() {
        tx.execute(
            "UPDATE zeroship.workflow_broadcasts \
                SET fanout_state = 'completed' \
              WHERE id = $1",
            &[&broadcast_id],
        )
        .await
        .map_err(RegistryError::from)?;
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(FanoutStats {
            broadcasts: 1,
            deliveries: 0,
        });
    }

    let subscribers = tx
        .query(
            "SELECT s.run_id, s.ordinal \
               FROM zeroship.workflow_subscriptions s \
               JOIN zeroship.workflow_runs r ON r.id = s.run_id AND r.app_id = s.app_id \
              WHERE s.app_id = $1 \
                AND s.topic = $2 \
                AND (s.type_filter IS NULL OR s.type_filter = $3) \
                AND (s.expires_at IS NULL OR s.expires_at >= now()) \
                AND r.state NOT IN ('completed', 'failed', 'cancelled', 'stalled', 'compensating') \
                AND NOT EXISTS ( \
                    SELECT 1 \
                      FROM zeroship.workflow_signals sig \
                     WHERE sig.broadcast_id = $4 \
                       AND sig.run_id = s.run_id \
                ) \
              ORDER BY s.created_at, s.id \
              LIMIT $5 \
              FOR UPDATE OF s SKIP LOCKED",
            &[&app_id, &topic, &signal_type, &broadcast_id, &max_deliveries],
        )
        .await
        .map_err(RegistryError::from)?;

    let mut deliveries = 0;
    let mut woken_run_ids = Vec::new();
    for subscriber in subscribers {
        let run_id: String = subscriber.get("run_id");
        let signal_id = typed_id::new_workflow_signal_id();
        let inserted = tx
            .execute(
                "INSERT INTO zeroship.workflow_signals \
                    (id, run_id, type, payload, origin, delivery, topic, broadcast_id, \
                     idempotency_key, provider) \
                 SELECT $1, $2, $3, $4, $5, 'topic', $6, $7, $8, $9 \
                  WHERE NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_signals \
                     WHERE broadcast_id = $7 AND run_id = $2 \
                  )",
                &[
                    &signal_id,
                    &run_id,
                    &signal_type,
                    &payload,
                    &origin,
                    &topic,
                    &broadcast_id,
                    &idempotency_key,
                    &provider,
                ],
            )
            .await;
        let inserted = match inserted {
            Ok(n) => n,
            Err(e) if e.code() == Some(&SqlState::UNIQUE_VIOLATION) => 0,
            Err(e) => return Err(RegistryError::from(e)),
        };
        if inserted > 0 {
            deliveries += 1;
            let woken = tx.execute(
                "UPDATE zeroship.workflow_runs \
                    SET wake_at = now() \
                  WHERE id = $1 AND state = 'waiting'",
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            if woken > 0 {
                woken_run_ids.push(run_id);
            }
        }
    }

    let remaining: i64 = tx
        .query_one(
            "SELECT COUNT(*)::bigint AS n \
               FROM zeroship.workflow_subscriptions s \
               JOIN zeroship.workflow_runs r ON r.id = s.run_id AND r.app_id = s.app_id \
              WHERE s.app_id = $1 \
                AND s.topic = $2 \
                AND (s.type_filter IS NULL OR s.type_filter = $3) \
                AND (s.expires_at IS NULL OR s.expires_at >= now()) \
                AND r.state NOT IN ('completed', 'failed', 'cancelled', 'stalled', 'compensating') \
                AND NOT EXISTS ( \
                    SELECT 1 \
                      FROM zeroship.workflow_signals sig \
                     WHERE sig.broadcast_id = $4 \
                       AND sig.run_id = s.run_id \
                )",
            &[&app_id, &topic, &signal_type, &broadcast_id],
        )
        .await
        .map_err(RegistryError::from)?
        .get("n");
    if remaining == 0 {
        tx.execute(
            "UPDATE zeroship.workflow_broadcasts \
                SET fanout_state = 'completed' \
              WHERE id = $1",
            &[&broadcast_id],
        )
        .await
        .map_err(RegistryError::from)?;
    }

    tx.commit().await.map_err(RegistryError::from)?;
    for run_id in woken_run_ids {
        super::workflow_engine::register_run_timer(state, &run_id).await?;
    }
    Ok(FanoutStats {
        broadcasts: 1,
        deliveries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_add_accumulates_broadcasts_and_deliveries() {
        let mut stats = FanoutStats {
            broadcasts: 1,
            deliveries: 2,
        };
        stats.add(FanoutStats {
            broadcasts: 3,
            deliveries: 5,
        });
        assert_eq!(
            stats,
            FanoutStats {
                broadcasts: 4,
                deliveries: 7,
            }
        );
    }
}
