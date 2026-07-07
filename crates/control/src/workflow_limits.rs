use serde_json::Value;
use uuid::Uuid;

use crate::registry::RegistryError;

pub(crate) const WORKFLOW_STATE_CAP_ERROR_CODE: &str = "workflow_state_cap_exceeded";

const FREE_WORKFLOW_JOURNAL_MAX_BYTES: i64 = 100 * 1024 * 1024;
const PAID_WORKFLOW_JOURNAL_MAX_BYTES: i64 = 1024 * 1024 * 1024;
const RUN_JOURNAL_LIMIT_FIELD: &str = "workflow_journal_max_bytes";
const APP_JOURNAL_LIMIT_FIELD: &str = "workflow_app_journal_max_bytes";

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

#[derive(Debug, Clone, Copy)]
pub(crate) struct WorkflowJournalLimits {
    pub run_max_bytes: i64,
    pub app_max_bytes: i64,
}

pub(crate) async fn limits_for_app<C>(
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
        return Err(RegistryError::NotFound(format!(
            "app {app_id} not found for workflow journal limits"
        )));
    };

    let plan_id: String = row.get("plan_id");
    let plan_name: Option<String> = row.get("name");
    let runtime_limits: Option<Value> = row.get("runtime_limits_json");
    let default = default_journal_cap(&plan_id, plan_name.as_deref());
    let run_max_bytes = runtime_limits
        .as_ref()
        .and_then(|json| positive_i64_field(json, RUN_JOURNAL_LIMIT_FIELD))
        .unwrap_or(default);
    let app_max_bytes = runtime_limits
        .as_ref()
        .and_then(|json| positive_i64_field(json, APP_JOURNAL_LIMIT_FIELD))
        .unwrap_or(default);

    Ok(WorkflowJournalLimits {
        run_max_bytes,
        app_max_bytes,
    })
}

pub(crate) async fn json_column_size<C>(conn: &C, value: &Value) -> Result<i64, RegistryError>
where
    C: compio_postgres::GenericClient + Sync,
{
    let rows = conn
        .query(
            "SELECT pg_column_size($1::jsonb)::bigint AS bytes",
            &[value],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows[0].get("bytes"))
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
    let rows = conn
        .query(
            "SELECT COALESCE(SUM(journal_bytes), 0)::bigint AS bytes \
               FROM zeroship.workflow_runs \
              WHERE app_id = $1",
            &[app_id],
        )
        .await
        .map_err(RegistryError::from)?;
    Ok(rows[0].get("bytes"))
}

pub(crate) fn cap_exceeded(current: i64, delta: i64, cap: i64) -> bool {
    i128::from(current) + i128::from(delta) > i128::from(cap)
}

pub(crate) fn state_cap_error(current: i64, delta: i64, cap: i64) -> Value {
    serde_json::json!({
        "type": "LimitExceededError",
        "error_code": WORKFLOW_STATE_CAP_ERROR_CODE,
        "message": "workflow journal state cap exceeded",
        "current_bytes": current,
        "delta_bytes": delta,
        "max_bytes": cap,
    })
}

fn default_journal_cap(plan_id: &str, plan_name: Option<&str>) -> i64 {
    if plan_name == Some("free") || plan_id == crate::bootstrap_console::free_plan_id() {
        FREE_WORKFLOW_JOURNAL_MAX_BYTES
    } else {
        PAID_WORKFLOW_JOURNAL_MAX_BYTES
    }
}

fn positive_i64_field(json: &Value, field: &str) -> Option<i64> {
    let value = json.get(field)?;
    match value {
        Value::Number(n) => n.as_i64().filter(|v| *v > 0),
        Value::String(s) => s.parse::<i64>().ok().filter(|v| *v > 0),
        _ => None,
    }
}
