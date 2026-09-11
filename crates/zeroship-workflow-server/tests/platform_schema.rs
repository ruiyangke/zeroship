//! Apply the actual platform corpus before inspecting the service's authority.

use std::{os::unix::fs::PermissionsExt, path::Path, process::Command, sync::Arc};
use testcontainers::{
    core::{IntoContainerPort, WaitFor},
    runners::SyncRunner,
    GenericImage, ImageExt,
};
use zeroship_workflow::service::{
    store::{PostgresStore, WorkflowStore},
    AppPolicy, PlatformPolicy, WorkflowService,
};

#[path = "support/server_process.rs"]
mod server_process;

async fn connect(url: &str) -> compio_postgres::Client {
    let (client, connection) = compio_postgres::connect(url, compio_postgres::NoTls)
        .await
        .unwrap();
    compio::runtime::spawn(async move { connection.run().await.unwrap() }).detach();
    client
}

#[ntex::test]
async fn platform_migration_provisions_workflow_authority_without_worker_access() {
    let postgres = GenericImage::new("postgres", "18")
        .with_exposed_port(5432.tcp())
        .with_wait_for(WaitFor::message_on_stdout(
            "PostgreSQL init process complete; ready for start up.",
        ))
        .with_wait_for(WaitFor::message_on_stderr(
            "database system is ready to accept connections",
        ))
        .with_env_var("POSTGRES_HOST_AUTH_METHOD", "trust")
        .start()
        .expect("workflow schema tests require Testcontainers PostgreSQL");
    let host = postgres.get_host().unwrap();
    let port = postgres.get_host_port_ipv4(5432).unwrap();
    let url = format!("postgres://postgres@{host}:{port}/postgres");
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let work = tempfile::tempdir().unwrap();
    let config = work.path().join("migrate.toml");
    let values = serde_json::json!({"env":{"platform":{
        "url":url,"dir":root.join("db/migrations-ts"),"schema":"zeroship","owner_app":"zeroship_platform",
        "registry":root.join("policies/platform-table-owners.json"),"policy":[root.join("policies/platform.policy.toml")],
    }}});
    std::fs::write(&config, toml::to_string(&values).unwrap()).unwrap();
    std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o600)).unwrap();
    let result = Command::new("node")
        .arg(root.join("packages/zero-migrate-cli/dist/cli-bin.js"))
        .args(["apply", "--config"])
        .arg(&config)
        .args(["--env", "platform", "--approve"])
        .current_dir(&root)
        .output()
        .expect("build the canonical migration CLI before testing");
    assert!(
        result.status.success(),
        "platform migration failed:\n{}\n{}",
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );

    let runtime_url = format!("postgres://zeroship_workflow@{host}:{port}/postgres");
    let store = Arc::new(PostgresStore::platform(
        runtime_url.clone(),
        PlatformPolicy::new(AppPolicy::default()).unwrap(),
    ));
    WorkflowService::open(store.clone()).await.unwrap();
    let runtime = connect(&runtime_url).await;
    for sql in [
        "SELECT id,plan_id,organization_id,workflows_enabled,archived_at,deleted_at FROM zeroship.apps",
        "SELECT id,workflows_allowed,archived,runtime_limits_json FROM zeroship.plans",
        "SELECT app_id,state FROM zeroship.app_spend_state",
        "SELECT organization_id,state FROM zeroship.organization_billing_status",
        "SELECT id,status,public_key FROM zeroship.worker_instances",
        "SELECT id,dispatch_paused,ingress_disabled FROM zeroship.workflow_rollout_config",
        "SELECT * FROM workflow.schema_version",
        "SELECT * FROM service_authn.service_assertion_replay",
        "BEGIN; SELECT zeroship.workflow_policy_lock('app','test',false); ROLLBACK",
    ] { runtime.batch_execute(sql).await.unwrap(); }
    for sql in [
        "UPDATE zeroship.apps SET workflows_enabled=true",
        "UPDATE zeroship.plans SET workflows_allowed=true",
        "SELECT * FROM zeroship.app_secrets",
        "SELECT * FROM zeroship.apps",
        "UPDATE workflow.schema_version SET fingerprint='forged'",
        "CREATE TABLE workflow.rogue(id text)",
        "ALTER TABLE workflow.app_state ADD COLUMN rogue text",
        "SET ROLE zeroship_workflow_migrator",
    ] {
        let error = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(
            error.code(),
            Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE),
            "{sql}: {error}"
        );
    }
    for role in [
        "zeroship_control",
        "zeroship_worker",
        "zeroship_gateway",
        "zeroship_app",
    ] {
        let peer = connect(&format!("postgres://{role}@{host}:{port}/postgres")).await;
        for sql in [
            "SELECT * FROM workflow.app_state",
            "SET ROLE zeroship_workflow_migrator",
            "SET ROLE zeroship_workflow",
        ] {
            assert!(peer.batch_execute(sql).await.is_err(), "{role}: {sql}");
        }
    }
    let admin = connect(&url).await;
    let owner: String = admin
        .query_one(
            "SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='workflow'",
            &[],
        )
        .await
        .unwrap()
        .get(0);
    assert_eq!(owner, "zeroship_workflow_migrator");
    let bad: i64 = admin.query_one("SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='workflow' AND pg_get_userbyid(c.relowner)<>'zeroship_workflow_migrator'",&[]).await.unwrap().get(0);
    assert_eq!(bad, 0);
    let rows = admin.query("SELECT p.proname,p.prosecdef FROM pg_proc p JOIN pg_namespace n ON n.oid=p.pronamespace WHERE n.nspname='zeroship' AND p.proname LIKE 'workflow_policy_%'",&[]).await.unwrap();
    assert!(!rows.is_empty());
    for row in rows {
        assert!(
            !row.get::<_, bool>(1),
            "{} must execute with invoker rights",
            row.get::<_, String>(0)
        );
    }
    for (break_policy, restore_policy) in [
        (
            "ALTER TABLE zeroship.apps DISABLE TRIGGER workflow_policy_fence_app",
            "ALTER TABLE zeroship.apps ENABLE TRIGGER workflow_policy_fence_app",
        ),
        (
            "GRANT zeroship_workflow_migrator TO zeroship_workflow",
            "REVOKE zeroship_workflow_migrator FROM zeroship_workflow",
        ),
        (
            "GRANT UPDATE ON workflow.schema_version TO zeroship_workflow",
            "REVOKE UPDATE ON workflow.schema_version FROM zeroship_workflow",
        ),
        (
            "DELETE FROM zeroship.workflow_rollout_config",
            "INSERT INTO zeroship.workflow_rollout_config (id) VALUES ('global')",
        ),
    ] {
        admin.batch_execute(break_policy).await.unwrap();
        assert!(
            store.verify().await.is_err(),
            "startup accepted: {break_policy}"
        );
        admin.batch_execute(restore_policy).await.unwrap();
        store.verify().await.unwrap();
    }
    server_process::contract(&admin, &runtime_url, work.path()).await;
}
