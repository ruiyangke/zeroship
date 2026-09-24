//! Startup seeding must preserve operator-owned workflow authority.
#![expect(clippy::future_not_send, reason = "native catalog tests use compio")]

#[allow(
    dead_code,
    reason = "the shared migrated platform also supports server tests"
)]
#[path = "../../zeroship-workflow-server/tests/support/platform.rs"]
mod platform;

use zeroship_control::{
    Registry,
    plan_catalog::{PlanCatalog, free_plan_id, seed_plans},
};
use zeroship_core::{schema_name::SchemaName, workflow_policy::AppPolicy};
use zeroship_data_orm::{
    ConnectOptions, binding::DbBinding, encryption::ProjectKeySource, orm::Database,
};
use zeroship_workflow_manager::policy::control::{self, PlanPolicyStore};

#[compio::test]
async fn startup_preserves_archived_plans_and_complete_workflow_policy() {
    let fixture = Box::pin(platform::Platform::new()).await;
    let url = fixture
        .runtime_url
        .replacen("zeroship_workflow@", "zeroship_control@", 1);
    let registry = Registry::new(&url).await.unwrap();
    seed_plans(&registry).await.unwrap();
    let catalog = PlanCatalog::new(registry.clone());
    catalog.archive(&free_plan_id()).await.unwrap();
    // Plan policy is Control's row, provisioned under the administrative
    // credential the runbook describes. No service login reaches it: the
    // workflow role lost its grant on `zeroship.plans` when the policy inputs
    // moved behind Control's app-facts endpoint.
    let operator_url = fixture
        .runtime_url
        .replacen("zeroship_workflow@", "postgres@", 1);
    let inputs = Database::connect(
        DbBinding::platform(
            "platform",
            "workflow-plan-admin",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(&operator_url, ProjectKeySource::unavailable())
            .connection_authority(),
        control::plan_admin_collections().unwrap(),
    )
    .await
    .unwrap();
    let plans = PlanPolicyStore::new(inputs).unwrap();
    plans.ready().await.unwrap();
    let policy = AppPolicy {
        max_live_runs: 17,
        ..AppPolicy::default()
    };
    plans.set_plan_policy(&free_plan_id(), &policy).await.unwrap();

    seed_plans(&registry).await.unwrap();
    assert!(
        catalog
            .get(&free_plan_id())
            .await
            .unwrap()
            .unwrap()
            .archived
    );
    let stored: serde_json::Value = fixture
        .admin
        .query_one(
            "SELECT workflow_policy_json FROM zeroship.plans WHERE id=$1",
            &[&free_plan_id()],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(serde_json::from_value::<AppPolicy>(stored).unwrap(), policy);
}
