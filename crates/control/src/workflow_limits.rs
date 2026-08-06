use uuid::Uuid;
use zeroship_plugin_workflow::engine::{
    workflow_journal_limits_from_plan, WorkflowJournalLimits,
};
use zeroship_plugin_workflow::store::pg::WorkflowTables;

use crate::registry::RegistryError;

#[allow(dead_code)]
pub(crate) mod operator_pending_g3_workflow_capacity {
    // operator-pending (G3): placeholder from a single-machine dev bench — operator must re-measure + sign off before GA.
    //
    // Documentation-only seed, not a certified GA limit. DW-23 measured a
    // 128-run backlog at concurrency 32: 10.237 dispatch claims/sec,
    // 10.054 checkpoint writes/sec, and 281.657 ms replay p95.
    pub(crate) const MAX_CONCURRENT_RUNS_PER_APP_PLACEHOLDER: i64 = 4;
    pub(crate) const MAX_DISPATCHES_PER_APP_PER_SEC_PLACEHOLDER: i64 = 2;
    pub(crate) const MAX_CHECKPOINT_WRITES_PER_APP_PER_SEC_PLACEHOLDER: i64 = 2;
}

pub(crate) async fn lock_app_journal_accounting<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<(), RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let key = format!("workflow-journal:{app_id}");
    conn.query("SELECT pg_advisory_xact_lock(hashtext($1)::bigint)", &[&key])
        .await
        .map_err(RegistryError::from)?;
    Ok(())
}

pub(crate) async fn app_journal_bytes<C>(conn: &C, app_id: &Uuid) -> Result<i64, RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let tables = WorkflowTables::for_app_id(app_id);
    let sql = format!(
        "SELECT COALESCE(SUM(journal_bytes), 0)::bigint AS bytes \
               FROM {runs}",
        runs = tables.runs
    );
    let rows = conn
        .query(&sql, &[])
        .await
        .map_err(RegistryError::from)?;
    Ok(rows[0].get("bytes"))
}

pub(crate) async fn workflow_journal_limits_for_app<C>(
    conn: &C,
    app_id: &Uuid,
) -> Result<WorkflowJournalLimits, RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT a.plan_id, p.name, p.runtime_limits_json \
               FROM zeroship.apps a \
               LEFT JOIN zeroship.plans p ON p.id = a.plan_id \
              WHERE a.id = $1",
            &[app_id],
        )
        .await
        .map_err(RegistryError::from)?;
    let Some(row) = rows.first() else {
        return Err(RegistryError::InvalidInput(format!(
            "app {app_id} not found for workflow journal limits"
        )));
    };
    let plan_id: String = row.get("plan_id");
    let plan_name: Option<String> = row.get("name");
    let runtime_limits: Option<serde_json::Value> = row.get("runtime_limits_json");
    Ok(workflow_journal_limits_from_plan(
        &plan_id,
        plan_name.as_deref(),
        runtime_limits.as_ref(),
    ))
}
