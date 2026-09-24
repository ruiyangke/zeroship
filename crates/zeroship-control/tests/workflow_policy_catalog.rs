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
use zeroship_workflow_manager::policy::control::{self, ControlPolicyStore};

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
    // The store spans Control's plan rows and the workflow service's own
    // publication schema, so it binds the administrative credential an operator
    // publishes with rather than either service login.
    let operator_url = fixture
        .runtime_url
        .replacen("zeroship_workflow@", "postgres@", 1);
    let inputs = Database::connect(
        DbBinding::platform(
            "platform",
            "workflow-policy",
            SchemaName::new("zeroship").unwrap(),
        ),
        ConnectOptions::new(&operator_url, ProjectKeySource::unavailable())
            .connection_authority(),
        control::collections().unwrap(),
    )
    .await
    .unwrap();
    let publication = Database::connect(
        DbBinding::platform(
            "workflow-policy-ledger",
            "workflow-policy-ledger",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        ConnectOptions::new(&operator_url, ProjectKeySource::unavailable())
            .connection_authority(),
        control::publication_collections().unwrap(),
    )
    .await
    .unwrap();
    let operator = ControlPolicyStore::new(inputs, publication).unwrap();
    let policy = AppPolicy {
        max_live_runs: 17,
        ..AppPolicy::default()
    };
    operator
        .set_plan_policy(&free_plan_id(), &policy)
        .await
        .unwrap();

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
