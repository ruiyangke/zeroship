//! Durable-workflow topic broadcast fan-out sweep.
//!
//! Broadcasts are durable rows. This sweep is a peer of the workflow claim
//! sweep: it claims pending broadcast rows with `SKIP LOCKED`, delivers normal
//! `workflow_signals` rows to matching subscriptions, and relies on the
//! `(broadcast_id, run_id)` marker to make re-drain idempotent after a crash.

use std::collections::BTreeSet;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use compio_postgres::error::SqlState;
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::typed_id;
use zeroship_plugin_workflow::store::pg::WorkflowTables;

use crate::cron::workflow_engine::SweepCoverage;
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
    /// How much of the journalled fleet the SUBSCRIPTION GC half of this tick
    /// covered.
    ///
    /// Scoped to that half deliberately. The broadcast drain below is not a
    /// fleet sweep - it claims the oldest pending broadcast whichever tenant
    /// owns it, and its own exclusion (an app whose journal schema it cannot
    /// enter) leaves the row PENDING rather than dropping it, so a stalled
    /// broadcast is recoverable state in the table rather than a lost tick.
    /// The GC half is the one that walks every app once and can silently cover
    /// fewer of them.
    pub coverage: SweepCoverage,
}

impl FanoutStats {
    /// Sums the WORK counts only.
    ///
    /// `coverage` is deliberately not summed: the subscription GC runs ONCE per
    /// tick, before the broadcast loop this is called from, so adding it per
    /// drained broadcast would multiply one sweep's account of the fleet by the
    /// number of broadcasts that happened to be pending.
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
            Ok(stats)
                if stats.broadcasts > 0
                    || stats.deliveries > 0
                    || stats.coverage.apps_skipped > 0 =>
            {
                tracing::info!(
                    broadcasts = stats.broadcasts,
                    deliveries = stats.deliveries,
                    apps_swept = stats.coverage.apps_swept,
                    apps_skipped = stats.coverage.apps_skipped,
                    "workflow_signal_fanout tick delivered broadcasts"
                );
            }
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
    stats.coverage = gc_expired_subscriptions(state).await?;
    for _ in 0..max_broadcasts {
        let drained = drain_one_broadcast(state, max_deliveries).await?;
        if drained.broadcasts == 0 {
            break;
        }
        stats.add(drained);
    }
    Ok(stats)
}

/// Drop every app's lapsed subscriptions, and report how much of the fleet that
/// covered.
///
/// The coverage matters more here than the row count does: a subscription that
/// outlives its expiry keeps receiving broadcasts, so an app excluded from this
/// sweep every tick delivers signals to runs that stopped listening. Nothing in
/// the delete count would ever say so.
async fn gc_expired_subscriptions(state: &AppState) -> Result<SweepCoverage, RegistryError> {
    let conn = state.registry.conn().await?;
    let fleet = super::workflow_engine::journalled_fleet(&conn).await?;
    let mut coverage = SweepCoverage::opened_over(&fleet);
    for app_id in fleet.readable {
        let tables = WorkflowTables::for_app_id(&app_id);
        let sql = format!(
            "DELETE FROM {} \
              WHERE expires_at IS NOT NULL AND expires_at < now()",
            tables.subscriptions
        );
        if super::workflow_engine::skip_journal_scoped(
            &app_id,
            "expired subscription gc",
            conn.execute(&sql, &[]).await,
        )?
        .is_some()
        {
            coverage.swept_one();
        } else {
            coverage.skipped_one();
        }
    }
    // No batch limit: every readable app is visited, so `apps_unvisited` is
    // structurally zero here.
    Ok(coverage)
}

/// Claim and deliver the OLDEST pending broadcast, whichever tenant it belongs
/// to.
///
/// The pick refuses a broadcast whose app has a journal schema it cannot enter.
/// Without that the sweep would claim the same broadcast every tick, fail on the
/// journal, and abort the fan-out for every other tenant behind it - a
/// fleet-wide stall from one app, which is the defect this file's sibling sweeps
/// were fixed for. The row stays PENDING rather than being marked completed,
/// because "I could not read the subscribers" is not "there were none"; the
/// broadcast drains as soon as the privilege gap is closed.
///
/// An app with NO journal schema at all is a different case and is deliberately
/// still picked: there are no subscribers to deliver to, and the arm below
/// completes the broadcast rather than leaving it forever pending. The skipped
/// app is named in the WARN that `journalled_fleet` emits from
/// `gc_expired_subscriptions` on the same tick, so the stall is not silent.
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
                    AND NOT EXISTS ( \
                        SELECT 1 \
                          FROM pg_catalog.pg_namespace n \
                         WHERE n.nspname = 'app_' || b.app_id::text \
                           AND NOT has_schema_privilege(n.oid, 'USAGE') \
                    ) \
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
    let Some(tables) = super::workflow_engine::existing_tables(&tx, &app_id).await? else {
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
            ..FanoutStats::default()
        });
    };

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
            ..FanoutStats::default()
        });
    }

    let subscribers = tx
        .query(
            &super::workflow_engine::journal_sql(
                &tables,
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
            ),
            &[&app_id, &topic, &signal_type, &broadcast_id, &max_deliveries],
        )
        .await
        .map_err(RegistryError::from)?;

    let mut deliveries = 0;
    let mut run_ids_to_register = BTreeSet::new();
    for subscriber in subscribers {
        let run_id: String = subscriber.get("run_id");
        let signal_id = typed_id::new_workflow_signal_id();
        let inserted = tx
            .execute(
                &super::workflow_engine::journal_sql(
                    &tables,
                "INSERT INTO zeroship.workflow_signals \
                    (id, run_id, type, payload, origin, delivery, topic, broadcast_id, \
                     idempotency_key, provider) \
                 SELECT $1, $2, $3, $4, $5, 'topic', $6, $7, $8, $9 \
                  WHERE NOT EXISTS ( \
                    SELECT 1 FROM zeroship.workflow_signals \
                     WHERE broadcast_id = $7 AND run_id = $2 \
                  )",
                ),
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
                &super::workflow_engine::journal_sql(
                    &tables,
                "UPDATE zeroship.workflow_runs \
                    SET wake_at = now() \
                  WHERE id = $1 AND state = 'waiting'",
                ),
                &[&run_id],
            )
            .await
            .map_err(RegistryError::from)?;
            if woken > 0 {
                run_ids_to_register.insert(run_id);
            }
        }
    }

    let remaining: i64 = tx
        .query_one(
            &super::workflow_engine::journal_sql(
                &tables,
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
            ),
            &[&app_id, &topic, &signal_type, &broadcast_id],
        )
        .await
        .map_err(RegistryError::from)?
        .get("n");
    let delivered_rows = tx
        .query(
            &super::workflow_engine::journal_sql(
                &tables,
                "SELECT DISTINCT sig.run_id \
                   FROM zeroship.workflow_signals sig \
                   JOIN zeroship.workflow_runs r ON r.id = sig.run_id AND r.app_id = $2 \
                  WHERE sig.broadcast_id = $1 \
                    AND r.state IN ('queued','running','sleeping','waiting','compensating') \
                    AND r.wake_at IS NOT NULL",
            ),
            &[&broadcast_id, &app_id],
        )
        .await
        .map_err(RegistryError::from)?;
    for row in delivered_rows {
        run_ids_to_register.insert(row.get("run_id"));
    }

    tx.commit().await.map_err(RegistryError::from)?;
    for run_id in run_ids_to_register {
        super::workflow_engine::register_run_timer(state, &run_id).await?;
    }
    if remaining == 0 {
        complete_broadcast_if_drained(state, &broadcast_id, app_id, &topic, &signal_type).await?;
    }
    Ok(FanoutStats {
        broadcasts: 1,
        deliveries,
        ..FanoutStats::default()
    })
}

async fn complete_broadcast_if_drained(
    state: &AppState,
    broadcast_id: &str,
    app_id: Uuid,
    topic: &str,
    signal_type: &str,
) -> Result<(), RegistryError> {
    let mut conn = state.registry.conn().await?;
    let tx = conn.transaction().await.map_err(RegistryError::from)?;
    let rows = tx
        .query(
            "SELECT fanout_state \
               FROM zeroship.workflow_broadcasts \
              WHERE id = $1 \
              FOR UPDATE",
            &[&broadcast_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(());
    };
    let fanout_state: String = row.get("fanout_state");
    if fanout_state == "completed" {
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(());
    }

    let Some(tables) = super::workflow_engine::existing_tables(&tx, &app_id).await? else {
        tx.execute(
            "UPDATE zeroship.workflow_broadcasts \
                SET fanout_state = 'completed' \
              WHERE id = $1",
            &[&broadcast_id],
        )
        .await
        .map_err(RegistryError::from)?;
        tx.commit().await.map_err(RegistryError::from)?;
        return Ok(());
    };

    let remaining: i64 = tx
        .query_one(
            &super::workflow_engine::journal_sql(
                &tables,
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
            ),
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_add_accumulates_broadcasts_and_deliveries() {
        let mut stats = FanoutStats {
            broadcasts: 1,
            deliveries: 2,
            ..FanoutStats::default()
        };
        stats.add(FanoutStats {
            broadcasts: 3,
            deliveries: 5,
            ..FanoutStats::default()
        });
        assert_eq!(
            stats,
            FanoutStats {
                broadcasts: 4,
                deliveries: 7,
                ..FanoutStats::default()
            }
        );
    }

    /// `add` must leave `coverage` alone.
    ///
    /// It is called once per DRAINED BROADCAST, while coverage is a per-TICK
    /// account of the fleet. Summing it would multiply the tick's `apps_swept`
    /// and `apps_skipped` by however many broadcasts happened to be pending -
    /// producing an `apps_total` larger than the fleet, from a field whose
    /// entire purpose is to account for exactly the fleet.
    ///
    /// WHAT THIS DOES NOT CATCH. It pins the arithmetic of one method, not the
    /// call sites: it says nothing about `tick_with_config` assigning coverage
    /// before the drain loop rather than after it, and nothing about
    /// `gc_expired_subscriptions` counting the right apps. It also cannot see
    /// the broadcast drain's OWN exclusion - a broadcast left pending because
    /// its app's schema could not be entered is not represented in this struct
    /// at all.
    #[test]
    fn stats_add_does_not_accumulate_coverage() {
        let one_tick_of_coverage = SweepCoverage {
            apps_swept: 3,
            apps_skipped: 1,
            apps_unvisited: 0,
        };
        let mut stats = FanoutStats {
            broadcasts: 1,
            deliveries: 2,
            coverage: one_tick_of_coverage,
        };
        stats.add(FanoutStats {
            broadcasts: 1,
            deliveries: 1,
            coverage: one_tick_of_coverage,
        });
        assert_eq!(
            stats.coverage, one_tick_of_coverage,
            "coverage is per tick; draining a second broadcast must not double it"
        );
        assert_eq!(stats.broadcasts, 2, "the work counts still accumulate");
    }
}
