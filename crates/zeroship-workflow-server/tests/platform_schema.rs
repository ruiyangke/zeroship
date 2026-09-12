//! Verify the actual platform migration and the coordinator's database authority.
#[path = "support/platform.rs"]
mod platform;

use zeroship_workflow_server::coordinator::{Coordinator, Options};

#[ntex::test]
async fn platform_role_can_coordinate_without_customer_or_journal_privileges() {
    let fixture = platform::Platform::new().await;
    let service = Coordinator::connect(&fixture.runtime_url, Options::default())
        .await
        .unwrap();
    service.verify().await.unwrap();
    let runtime = platform::connect(&fixture.runtime_url).await;
    fixture
        .admin
        .batch_execute(
            "CREATE SCHEMA customer;
         CREATE TABLE customer.__zeroship_workflow_runs(secret text);
         INSERT INTO customer.__zeroship_workflow_runs VALUES('private-customer-input');",
        )
        .await
        .unwrap();
    for sql in [
        "SELECT app_id FROM workflow_coordination.scopes",
        "SELECT id,status,public_key FROM zeroship.worker_instances",
        "SELECT replay_key FROM service_authn.service_assertion_replay",
    ] {
        runtime.batch_execute(sql).await.unwrap();
    }
    for sql in [
        "SELECT * FROM customer.__zeroship_workflow_runs",
        "SELECT * FROM zeroship.workflow_runs",
        "SELECT * FROM zeroship.workflow_steps",
        "SELECT * FROM zeroship.apps",
        "SELECT * FROM zeroship.app_deploys",
        "SELECT * FROM zeroship.plans",
        "UPDATE zeroship.worker_instances SET status='active'",
        "UPDATE workflow_coordination.schema_version SET fingerprint='forged'",
        "CREATE TABLE workflow_coordination.extra(data text)",
        "SET ROLE zeroship_workflow_migrator",
    ] {
        assert!(
            runtime.batch_execute(sql).await.is_err(),
            "coordinator accepted {sql}"
        );
    }
    let schema = fixture
        .admin
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname='workflow'",
            &[],
        )
        .await
        .unwrap();
    assert!(
        schema.is_empty(),
        "central execution journal was provisioned"
    );
    let policies = fixture.admin.query("SELECT proname FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='zeroship' AND proname LIKE 'workflow_policy_%'",&[]).await.unwrap();
    assert!(
        policies.is_empty(),
        "central journal policy fences were provisioned"
    );
    for (grant, revoke) in [
        (
            "GRANT zeroship_workflow_migrator TO zeroship_workflow",
            "REVOKE zeroship_workflow_migrator FROM zeroship_workflow",
        ),
        (
            "GRANT pg_read_all_data TO zeroship_workflow",
            "REVOKE pg_read_all_data FROM zeroship_workflow",
        ),
        (
            "GRANT UPDATE ON workflow_coordination.schema_version TO zeroship_workflow",
            "REVOKE UPDATE ON workflow_coordination.schema_version FROM zeroship_workflow",
        ),
        (
            "GRANT CREATE ON SCHEMA workflow_coordination TO zeroship_workflow",
            "REVOKE CREATE ON SCHEMA workflow_coordination FROM zeroship_workflow",
        ),
    ] {
        fixture.admin.batch_execute(grant).await.unwrap();
        assert!(service.verify().await.is_err(), "startup accepted {grant}");
        fixture.admin.batch_execute(revoke).await.unwrap();
        service.verify().await.unwrap();
    }
    assert!(fixture.work.path().join("migrate.toml").is_file());
}
