use super::platform;
use zeroship_core::{AppId, schema_name::SchemaName, workflow_policy::AppPolicy};
use zeroship_data_orm::{
    ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
};
use zeroship_workflow_manager::policy::control::{self, ControlPolicyStore, RolloutPolicy};

/// An app, created on demand, whose plan and app flag both allow workflows.
/// Returns the plan that carries the app's workflow policy.
pub async fn seed_app(platform: &platform::Platform, app: &AppId) -> String {
    let plan = platform.seed_app(app).await;
    platform.admin.execute("UPDATE zeroship.apps SET workflows_enabled=true WHERE id=$1", &[&app.as_str()]).await.unwrap();
    plan
}

/// The policy store bound as Control, which owns these rows. The workflow
/// service reads them and cannot write them, so a fixture that publishes
/// policy has to connect under the role a deployment publishes it with.
#[allow(
    dead_code,
    reason = "targets that only read policy bind the service role instead"
)]
pub async fn operator(platform: &platform::Platform) -> ControlPolicyStore {
    let url = platform
        .runtime_url
        .replacen("zeroship_workflow@", "zeroship_control@", 1);
    let database = Database::connect(
        DbBinding::platform(
            "platform",
            "workflow-policy",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(&url, ProjectKeySource::unavailable()).connection_authority(),
        control::collections().unwrap(),
    )
    .await
    .unwrap();
    ControlPolicyStore::new(database).unwrap()
}

/// The operator switches a deployment publishes alongside its plan policies.
/// The migration corpus creates the table and no row: an observation joins the
/// `global` row, so an app whose deployment never published one is unavailable.
#[allow(dead_code, reason = "rollout cases publish their own switches")]
pub const fn rollout() -> RolloutPolicy {
    RolloutPolicy {
        dispatch_paused: false,
        ingress_disabled: false,
        source_validity_ms: 30_000,
    }
}

/// Seed the app AND publish the operator policy its plan carries.
///
/// A claim reads the app's delivery ceiling from the policy source before it
/// delivers, so an app whose plan carries no policy - or a deployment with no
/// published rollout switches - has no ceiling and every claim is refused
/// `unavailable`. Fixtures that deliver jobs provision both, the way a
/// deployment does, rather than relying on a default the source does not carry.
#[allow(dead_code, reason = "policy-source cases publish their own revisions")]
pub async fn provision(platform: &platform::Platform, app: &AppId, policy: &AppPolicy) -> String {
    let plan = seed_app(platform, app).await;
    let operator = operator(platform).await;
    Box::pin(operator.set_plan_policy(&plan, policy))
        .await
        .unwrap();
    Box::pin(operator.set_rollout(rollout())).await.unwrap();
    plan
}
