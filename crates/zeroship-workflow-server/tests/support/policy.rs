#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use super::{app_facts::DatabaseAppFacts, platform};
use zeroship_core::{AppId, schema_name::SchemaName};
use zeroship_data_orm::{
    ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
};
use zeroship_workflow_manager::policy::control::{
    self, ControlPolicyStore, PlanPolicyStore, RolloutPolicy,
};

/// An app, created on demand, whose plan and app flag both allow workflows.
/// Returns the plan that carries the app's workflow policy.
pub async fn seed_app(platform: &platform::Platform, app: &AppId) -> String {
    let plan = platform.seed_app(app).await;
    platform.admin.execute("UPDATE zeroship.apps SET workflows_enabled=true WHERE id=$1", &[&app.as_str()]).await.unwrap();
    plan
}

/// The administrative URL a deployment provisions plan policy under. No
/// service login reaches both halves: the workflow role reads its own switches
/// and cannot write them, holds nothing at all on `zeroship.plans` since the
/// policy inputs moved behind Control's endpoint, and `zeroship_control` cannot
/// reach the `workflow_manager` schema.
fn admin_url(platform: &platform::Platform) -> String {
    platform
        .runtime_url
        .replacen("zeroship_workflow@", "postgres@", 1)
}

/// Control's plan rows under the administrative credential. This is the
/// operator path `docs/runbooks/workflows.md` describes, and the only writer of
/// `zeroship.plans.workflow_policy_json` outside explicit SQL.
pub async fn plan_admin(platform: &platform::Platform) -> PlanPolicyStore {
    let inputs = Database::connect(
        DbBinding::platform(
            "platform",
            "workflow-plan-admin",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(admin_url(platform), ProjectKeySource::unavailable())
            .connection_authority(),
        control::plan_admin_collections().unwrap(),
    )
    .await
    .unwrap();
    PlanPolicyStore::new(inputs).unwrap()
}

/// The publication store bound with the administrative credential, for the
/// rollout switches a deployment publishes. Its facts come from the fixture's
/// own database source; a case that needs to dictate them binds
/// `ScriptedAppFacts` instead.
pub async fn operator(platform: &platform::Platform) -> ControlPolicyStore {
    let url = admin_url(platform);
    let publication = Database::connect(
        DbBinding::platform(
            "workflow-policy-ledger",
            "workflow-policy-ledger",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        ConnectOptions::new(&url, ProjectKeySource::unavailable()).connection_authority(),
        control::publication_collections().unwrap(),
    )
    .await
    .unwrap();
    ControlPolicyStore::new(DatabaseAppFacts::connect(&url).await, publication).unwrap()
}

/// The operator switches a deployment publishes alongside its plan policies.
/// The migration corpus creates the table and no row: an observation joins the
/// `global` row, so an app whose deployment never published one is unavailable.
pub const fn rollout() -> RolloutPolicy {
    RolloutPolicy {
        dispatch_paused: false,
        ingress_disabled: false,
        source_validity_ms: 30_000,
    }
}
