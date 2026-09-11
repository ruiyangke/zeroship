//! Authoritative platform policy, resolved under transaction-scoped writer fences.

use super::{app::not_found, store::Transaction, AppPolicy};
use crate::WorkflowServiceError;
use serde::Deserialize;
use zeroship_core::{app_derivation, app_id::AppId};

/// Operator limits cap the plan's workflow limits. Neither is supplied by an app.
#[derive(Debug, Clone)]
pub struct PlatformPolicy {
    ceiling: AppPolicy,
}

impl PlatformPolicy {
    pub fn new(ceiling: AppPolicy) -> Result<Self, WorkflowServiceError> {
        ceiling.validate()?;
        Ok(Self { ceiling })
    }

    pub(crate) async fn verify(tx: &mut Transaction) -> Result<(), WorkflowServiceError> {
        let rows = tx
            .query(
                "SELECT fingerprint FROM workflow.schema_version WHERE id='platform_policy'",
                &[],
            )
            .await?;
        if rows.len() != 1
            || rows[0].text("fingerprint")? != super::schema::fingerprint("platform_policy")?
        {
            return Err(super::schema::incompatible());
        }
        let rows = tx.query(
            "SELECT CASE WHEN NOT (rolsuper OR rolcreaterole OR rolcreatedb OR rolreplication OR rolbypassrls) \
             AND NOT pg_has_role(current_user,'zeroship_workflow_migrator','MEMBER') \
             AND NOT has_schema_privilege(current_user,'workflow','CREATE') \
             AND NOT has_any_column_privilege(current_user,'workflow.schema_version','UPDATE') \
             AND NOT has_table_privilege(current_user,'workflow.schema_version','INSERT,DELETE,TRUNCATE') \
             THEN 1 ELSE 0 END::bigint AS safe FROM pg_roles WHERE rolname=current_user",
            &[],
        ).await?;
        if rows.len() != 1 || rows[0].integer("safe")? != 1 {
            return Err(WorkflowServiceError::Unavailable(
                "workflow database role holds owner or administrative privileges".into(),
            ));
        }
        let rows = tx.query(
            "WITH expected(kind,relation) AS (VALUES ('app','apps'),('plan','plans'), \
             ('organization','organization_billing_status'),('spend','app_spend_state'),('rollout','workflow_rollout_config'),('deploy','app_deploys')) \
             SELECT CASE WHEN EXISTS (SELECT 1 FROM pg_trigger t JOIN pg_proc p ON p.oid=t.tgfoid \
             WHERE t.tgrelid=to_regclass('zeroship.' || e.relation) AND t.tgname='workflow_policy_fence_' || e.kind \
             AND t.tgenabled IN ('O','A') AND NOT p.prosecdef \
             AND t.tgtype=31 AND t.tgqual IS NULL AND t.tgnargs=0 AND cardinality(t.tgattr::smallint[])=0 \
             AND p.oid=to_regprocedure('zeroship.workflow_policy_fence_' || e.kind || '()')) \
             THEN 1 ELSE 0 END::bigint AS safe FROM expected e",
            &[],
        ).await?;
        if rows.is_empty() || rows.iter().any(|row| row.integer("safe").ok() != Some(1)) {
            return Err(WorkflowServiceError::Unavailable(
                "workflow policy writer fences are missing or disabled".into(),
            ));
        }
        let rows = tx
            .query(
                "SELECT CASE WHEN NOT prosecdef THEN 1 ELSE 0 END::bigint AS safe FROM pg_proc \
             WHERE oid=to_regprocedure('zeroship.workflow_policy_lock(text,text,boolean)')",
                &[],
            )
            .await?;
        if rows.len() != 1 || rows[0].integer("safe")? != 1 {
            return Err(unavailable());
        }
        for sql in [
            "SELECT id,plan_id,organization_id,workflows_enabled,archived_at,deleted_at,deploy_hash,manifest_json FROM zeroship.apps LIMIT 0",
            "SELECT id,app_id,deploy_hash,manifest_json,activated_at FROM zeroship.app_deploys LIMIT 0",
            "SELECT id,workflows_allowed,archived,runtime_limits_json FROM zeroship.plans LIMIT 0",
            "SELECT app_id,state FROM zeroship.app_spend_state LIMIT 0",
            "SELECT organization_id,state FROM zeroship.organization_billing_status LIMIT 0",
            "SELECT id,status,public_key FROM zeroship.worker_instances LIMIT 0",
            "SELECT zeroship.workflow_policy_lock('rollout','global',false)",
        ] { tx.execute(sql, &[]).await?; }
        let rows = tx
            .query(
                "SELECT id FROM zeroship.workflow_rollout_config WHERE id='global'",
                &[],
            )
            .await?;
        if rows.len() != 1 {
            return Err(unavailable());
        }
        Ok(())
    }

    pub(crate) async fn lock(
        &self,
        tx: &mut Transaction,
        app: &AppId,
    ) -> Result<AppPolicy, WorkflowServiceError> {
        let uuid = app.uuid();
        // Archive already owns the exclusive half of this protocol. Take it
        // before policy locks, matching Control's lifecycle write order.
        tx.execute(
            "SELECT pg_advisory_xact_lock_shared(hashtextextended($1, 0))",
            &[app_derivation::lifecycle_lock_seed_for_stored_uuid(&uuid).into()],
        )
        .await?;
        let id = uuid.to_string();
        lock_resource(tx, "app", &id).await?;
        let rows = tx
            .query(
                "SELECT plan_id, organization_id, \
             CASE WHEN workflows_enabled AND archived_at IS NULL AND deleted_at IS NULL \
             THEN 1 ELSE 0 END::bigint AS enabled FROM zeroship.apps WHERE id=$1::text::uuid",
                &[id.clone().into()],
            )
            .await?;
        let row = rows.first().ok_or_else(|| not_found("workflow app"))?;
        let plan = row.text("plan_id")?;
        let organization = row.text("organization_id")?;
        let enabled = row.integer("enabled")? == 1;
        // The app fence freezes these resource identities. Their writer fences
        // also cover INSERT into currently absent spend and billing rows.
        for (kind, identity) in [
            ("plan", plan.as_str()),
            ("organization", organization.as_str()),
            ("spend", id.as_str()),
            ("rollout", "global"),
        ] {
            lock_resource(tx, kind, identity).await?;
        }
        let rows = tx
            .query(
                "SELECT runtime_limits_json::text AS limits, \
             CASE WHEN workflows_allowed AND NOT archived THEN 1 ELSE 0 END::bigint AS allowed \
             FROM zeroship.plans WHERE id=$1",
                &[plan.into()],
            )
            .await?;
        let row = rows.first().ok_or_else(unavailable)?;
        let mut policy = self.apply_limits(&row.text("limits")?)?;
        policy.admission &= enabled && row.integer("allowed")? == 1;
        let rows = tx.query(
            "SELECT state::text AS state FROM zeroship.app_spend_state WHERE app_id=$1::text::uuid",
            &[id.into()],
        ).await?;
        // No spend state means no threshold has been reached. Unknown states
        // are never interpreted as permission to execute.
        if let Some(row) = rows.first() {
            policy.admission &= matches!(row.text("state")?.as_str(), "allow" | "warn" | "degrade");
        }
        let rows = tx.query(
            "SELECT state::text AS state FROM zeroship.organization_billing_status WHERE organization_id=$1",
            &[organization.into()],
        ).await?;
        // Billing creates this row on the first state transition.
        if let Some(row) = rows.first() {
            policy.admission &= matches!(row.text("state")?.as_str(), "active" | "past_due");
        }
        let rows = tx
            .query(
                "SELECT CASE WHEN dispatch_paused THEN 0 ELSE 1 END::bigint AS dispatch, \
             CASE WHEN ingress_disabled THEN 0 ELSE 1 END::bigint AS ingress \
             FROM zeroship.workflow_rollout_config WHERE id='global'",
                &[],
            )
            .await?;
        let row = rows.first().ok_or_else(unavailable)?;
        policy.dispatch &= row.integer("dispatch")? == 1;
        policy.ingress &= row.integer("ingress")? == 1;
        Ok(policy)
    }

    fn apply_limits(&self, json: &str) -> Result<AppPolicy, WorkflowServiceError> {
        let runtime: serde_json::Value = serde_json::from_str(json).map_err(|_| unavailable())?;
        let runtime = runtime.as_object().ok_or_else(unavailable)?;
        let limits: PlanLimits = match runtime.get("workflow") {
            Some(value) => serde_json::from_value(value.clone()).map_err(|_| unavailable())?,
            None => PlanLimits::default(),
        };
        let mut policy = self.ceiling.clone();
        macro_rules! cap {
            ($($field:ident),+ $(,)?) => {$ (
                if let Some(value) = limits.$field {
                    policy.$field = policy.$field.min(value);
                }
            )+};
        }
        cap!(
            max_live_runs,
            max_child_depth,
            max_running,
            max_input_bytes,
            max_frontier,
            max_journal_bytes,
            max_payload_bytes,
            max_payload_objects,
            max_payload_storage_bytes,
            max_compensation_attempts,
            max_schedules,
            max_schedule_backfill,
            max_signal_token_lifetime_seconds
        );
        if let Some(value) = limits.min_schedule_interval_ms {
            if value <= 0 {
                return Err(unavailable());
            }
            policy.min_schedule_interval_ms = policy.min_schedule_interval_ms.max(value);
        }
        policy.max_payload_bytes = policy
            .max_payload_bytes
            .min(policy.max_payload_storage_bytes);
        policy.validate().map_err(|_| unavailable())?;
        Ok(policy)
    }
}

async fn lock_resource(
    tx: &mut Transaction,
    kind: &str,
    identity: &str,
) -> Result<(), WorkflowServiceError> {
    tx.execute(
        "SELECT zeroship.workflow_policy_lock($1,$2,false)",
        &[kind.into(), identity.into()],
    )
    .await?;
    Ok(())
}

fn unavailable() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("workflow platform policy is missing or invalid".into())
}

/// The workflow object within a plan's runtime_limits_json. Durability and
/// authentication settings belong to the operator and cannot be plan overrides.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PlanLimits {
    max_live_runs: Option<i64>,
    max_child_depth: Option<i64>,
    max_running: Option<i64>,
    max_input_bytes: Option<usize>,
    max_frontier: Option<usize>,
    max_journal_bytes: Option<usize>,
    max_payload_bytes: Option<i64>,
    max_payload_objects: Option<i64>,
    max_payload_storage_bytes: Option<i64>,
    max_compensation_attempts: Option<i32>,
    max_schedules: Option<usize>,
    max_schedule_backfill: Option<usize>,
    min_schedule_interval_ms: Option<i64>,
    max_signal_token_lifetime_seconds: Option<i64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_limits_can_only_restrict_the_operator_ceiling() {
        let source = PlatformPolicy::new(AppPolicy {
            max_running: 7,
            ..AppPolicy::default()
        })
        .unwrap();
        assert_eq!(
            source
                .apply_limits(r#"{"workflow":{"maxRunning":99}}"#)
                .unwrap()
                .max_running,
            7
        );
        assert_eq!(
            source
                .apply_limits(r#"{"workflow":{"maxRunning":2}}"#)
                .unwrap()
                .max_running,
            2
        );
        assert_eq!(
            source
                .apply_limits(r#"{"workflow":{"maxRunning":0}}"#)
                .unwrap()
                .max_running,
            0
        );
        for json in [
            "null",
            "[]",
            r#"{"workflow":null}"#,
            r#"{"workflow":{"maxRunning":-1}}"#,
            r#"{"workflow":{"maxInputBytes":0}}"#,
            r#"{"workflow":{"maxRunning":"2"}}"#,
            r#"{"workflow":{"admission":true}}"#,
            r#"{"workflow":{"leaseMs":1}}"#,
            r#"{"workflow":{"maxRuning":1}}"#,
            r#"{"workflow":{"minScheduleIntervalMs":-1}}"#,
        ] {
            assert!(
                matches!(
                    source.apply_limits(json),
                    Err(WorkflowServiceError::Unavailable(_))
                ),
                "{json}"
            );
        }
    }
}
