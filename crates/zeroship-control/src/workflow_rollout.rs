//! Durable-workflow rollout gates.
//!
//! Enablement and operator switches live in the control database so every
//! control-plane replica observes the same rollout state.

use compio_postgres::GenericClient;
use zeroship_core::app_derivation;
use zeroship_core::app_id::AppId;

use crate::registry::RegistryError;

pub const ROLLOUT_CONFIG_ID: &str = "global";

pub async fn workflows_enabled_for_app<C>(
    conn: &C,
    app_id: &AppId,
) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    // Archive takes the exclusive form of this transaction-scoped lock. Calls
    // made inside a workflow mutation transaction therefore either commit
    // before archive returns or observe the archived marker afterwards.
    conn.query_one(
        "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))",
        &[&app_derivation::lifecycle_lock_seed(app_id)],
    )
    .await
    .map_err(RegistryError::from)?;
    let rows = conn
        .query(
            "SELECT a.workflows_enabled \
                    AND COALESCE(p.workflows_allowed, false) \
                    AND NOT COALESCE(p.archived, true) \
                    AND a.archived_at IS NULL AS enabled \
               FROM zeroship.apps a \
               LEFT JOIN zeroship.plans p ON p.id = a.plan_id \
              WHERE a.id = $1",
            &[&app_id.as_str()],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .first()
        .is_some_and(|row| row.get::<_, bool>("enabled")))
}

pub async fn dispatch_paused<C>(conn: &C) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    rollout_bool(conn, "dispatch_paused").await
}

pub async fn ingress_disabled<C>(conn: &C) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    rollout_bool(conn, "ingress_disabled").await
}

async fn rollout_bool<C>(conn: &C, column: &str) -> Result<bool, RegistryError>
where
    C: GenericClient + Sync,
{
    let query = match column {
        "dispatch_paused" => {
            "SELECT dispatch_paused AS enabled \
               FROM zeroship.workflow_rollout_config \
              WHERE id = $1"
        }
        "ingress_disabled" => {
            "SELECT ingress_disabled AS enabled \
               FROM zeroship.workflow_rollout_config \
              WHERE id = $1"
        }
        _ => unreachable!("rollout bool column is fixed by caller"),
    };
    let rows = conn
        .query(query, &[&ROLLOUT_CONFIG_ID])
        .await
        .map_err(RegistryError::from)?;
    Ok(rows
        .first()
        .is_some_and(|row| row.get::<_, bool>("enabled")))
}
