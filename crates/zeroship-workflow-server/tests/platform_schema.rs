//! Verify platform provisioning and workflow metadata database authority.
#[path = "support/holds.rs"]
mod holds;
#[path = "support/platform.rs"]
mod platform;

use zeroship_workflow_server::coordinator::{Coordinator, Options};

#[ntex::test]
async fn platform_role_can_coordinate_without_customer_or_journal_privileges() {
    let fixture = platform::Platform::new().await;
    let service = Coordinator::connect(&fixture.runtime_url, Options::default(), holds::client())
        .await
        .unwrap();
    service.verify().await.unwrap();
    let runtime = platform::connect(&fixture.runtime_url).await;
    fixture
        .admin
        .batch_execute(
            "CREATE SCHEMA customer;
         CREATE TABLE customer.__zeroship_workflow_runs(id text PRIMARY KEY,secret text);",
        )
        .await
        .unwrap();
    fixture
        .admin
        .execute(
            "INSERT INTO customer.__zeroship_workflow_runs(id,secret) VALUES($1,'private-customer-input')",
            &[&zeroship_core::workflow_coordination::RunId::mint().as_str()],
        )
        .await
        .unwrap();
    for sql in [
        "SELECT id FROM workflow_manager.queue_scopes",
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
        "UPDATE workflow_manager.schema_version SET fingerprint='forged'",
        "CREATE TABLE workflow_manager.extra(id text PRIMARY KEY,data text)",
        "SET ROLE zeroship_workflow_migrator",
    ] {
        assert!(
            runtime.batch_execute(sql).await.is_err(),
            "coordinator accepted {sql}"
        );
    }
    manager_queue_authority(&fixture, &runtime).await;
    manager_recovery_authority(&fixture).await;
    manager_scheduling_authority(&fixture).await;
    let schema = fixture
        .admin
        .query(
            "SELECT nspname FROM pg_namespace WHERE nspname IN ('workflow','workflow_coordination')",
            &[],
        )
        .await
        .unwrap();
    assert!(
        schema.is_empty(),
        "separate workflow schemas were provisioned"
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
            "GRANT UPDATE ON workflow_manager.schema_version TO zeroship_workflow",
            "REVOKE UPDATE ON workflow_manager.schema_version FROM zeroship_workflow",
        ),
        (
            "GRANT CREATE ON SCHEMA workflow_manager TO zeroship_workflow",
            "REVOKE CREATE ON SCHEMA workflow_manager FROM zeroship_workflow",
        ),
        (
            "GRANT TRIGGER ON workflow_manager.deployment_holds TO zeroship_workflow",
            "REVOKE TRIGGER ON workflow_manager.deployment_holds FROM zeroship_workflow",
        ),
    ] {
        fixture.admin.batch_execute(grant).await.unwrap();
        assert!(service.verify().await.is_err(), "startup accepted {grant}");
        fixture.admin.batch_execute(revoke).await.unwrap();
        service.verify().await.unwrap();
    }
    fixture
        .admin
        .batch_execute("REVOKE UPDATE ON workflow_manager.deployment_holds FROM zeroship_workflow")
        .await
        .unwrap();
    assert!(service.verify().await.is_err());
    fixture
        .admin
        .batch_execute("GRANT UPDATE ON workflow_manager.deployment_holds TO zeroship_workflow")
        .await
        .unwrap();
    service.verify().await.unwrap();
    assert!(fixture.work.path().join("migrate.toml").is_file());
}

#[expect(
    clippy::future_not_send,
    reason = "native recovery operations stay on the host's compio thread"
)]
async fn manager_recovery_authority(fixture: &platform::Platform) {
    use zeroship_core::{
        app_id::AppId,
        schema_name::SchemaName,
        workflow_jobs::{DeploymentId, JobOperation},
    };
    use zeroship_data_orm::binding::DbBinding;
    use zeroship_workflow_manager::{
        recovery::{Options as RecoveryOptions, Recovery},
        Options as QueueOptions, Queue,
    };

    let queue = Queue::connect(
        DbBinding::new(
            "workflow_manager",
            "platform-schema",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        &fixture.runtime_url,
        QueueOptions::default(),
        holds::client(),
    )
    .await
    .unwrap();
    let recovery = Recovery::new(queue, RecoveryOptions::default()).unwrap();
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    recovery
        .ensure(&app, &deployment, 1.try_into().unwrap())
        .await
        .unwrap();
    assert_eq!(
        recovery.due(None).await.unwrap(),
        std::slice::from_ref(&app)
    );
    let job = recovery.dispatch(&app).await.unwrap().unwrap();
    assert_eq!(job.app_id, app);
    assert_eq!(job.deployment_id, deployment);
    assert_eq!(job.operation, JobOperation::Reconcile {});
    assert_eq!(recovery.dispatch(&app).await.unwrap(), Some(job));
}

#[expect(
    clippy::future_not_send,
    reason = "native scheduling uses the platform role through compio ORM"
)]
async fn manager_scheduling_authority(fixture: &platform::Platform) {
    use zeroship_core::{
        app_id::AppId,
        schema_name::SchemaName,
        workflow_jobs::{DeploymentId, JobOperation},
        workflow_schedules::{
            ActivateSchedules, RegisterSchedules, ScheduleCatchUp, ScheduleDescriptor,
            ScheduleOverlap, ScheduleTiming,
        },
    };
    use zeroship_data_orm::binding::DbBinding;
    use zeroship_workflow_manager::{
        scheduling::{Options as SchedulingOptions, Scheduler},
        Options as QueueOptions, Queue,
    };

    let queue = Queue::connect(
        DbBinding::new(
            "workflow_manager",
            "platform-schema",
            SchemaName::new("workflow_manager").unwrap(),
        ),
        &fixture.runtime_url,
        QueueOptions::default(),
        holds::client(),
    )
    .await
    .unwrap();
    let scheduler = Scheduler::new(queue, SchedulingOptions::default()).unwrap();
    let app = AppId::mint();
    let deployment = DeploymentId::mint();
    scheduler
        .prepare(&RegisterSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            schedules: vec![ScheduleDescriptor {
                name: "daily".into(),
                workflow_name: "report".into(),
                schedule: ScheduleTiming::Cron {
                    cron_expr: "@daily".into(),
                    tz: "UTC".into(),
                },
                overlap: ScheduleOverlap::default(),
                catch_up: ScheduleCatchUp::default(),
            }],
        })
        .await
        .unwrap();
    let activation = scheduler
        .activate(&ActivateSchedules {
            app_id: app.clone(),
            deployment_id: deployment.clone(),
            revision: 1.try_into().unwrap(),
        })
        .await
        .unwrap();
    assert!(matches!(
        activation.operation,
        JobOperation::Activate { .. }
    ));
    // Seed a historical frontier without waiting for the wall clock to reach it.
    fixture
        .admin
        .execute(
            "UPDATE workflow_manager.schedules SET next_at=0 WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let due = scheduler.due(None).await.unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].app_id, app);
    let page = scheduler.dispatch(&app, &due[0].schedule_id).await.unwrap();
    assert_eq!(page.jobs.len(), 1);
    assert_eq!(page.jobs[0].deployment_id, deployment);
    assert!(matches!(page.jobs[0].operation, JobOperation::Cron { .. }));
    assert!(!page.more);
    assert!(scheduler.due(None).await.unwrap().is_empty());
}

#[expect(
    clippy::too_many_lines,
    reason = "the provisioned role's grants and refusals are checked against the same queue state"
)]
async fn manager_queue_authority(fixture: &platform::Platform, runtime: &compio_postgres::Client) {
    use zeroship_core::{
        app_id::AppId,
        workflow_jobs::{DeploymentId, JobId, JobOperation},
    };
    let namespace = fixture
        .admin
        .query_one(
            "SELECT pg_get_userbyid(nspowner) FROM pg_namespace WHERE nspname='workflow_manager'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(namespace.get::<_, String>(0), "zeroship_workflow_migrator");
    let tables = fixture.admin.query(
        "SELECT c.relname, pg_get_userbyid(c.relowner) FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='workflow_manager' AND c.relkind='r' ORDER BY c.relname",
        &[],
    ).await.unwrap();
    assert_eq!(
        tables
            .iter()
            .map(|row| row.get::<_, String>(0))
            .collect::<Vec<_>>(),
        [
            "assignments",
            "deployment_holds",
            "jobs",
            "management",
            "placement_receipts",
            "queue_scopes",
            "recovery_scopes",
            "schedule_activations",
            "schedule_deployments",
            "schedule_occurrences",
            "schedule_scopes",
            "schedules",
            "schema_version",
            "workers"
        ]
    );
    for table in &tables {
        assert_eq!(table.get::<_, String>(1), "zeroship_workflow_migrator");
    }
    let version = runtime
        .query_one(
            "SELECT fingerprint FROM workflow_manager.schema_version WHERE id='manager'",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        version.get::<_, String>(0),
        include_str!("../../zeroship-workflow-manager/schema/fingerprint.txt").trim()
    );

    let app = AppId::mint();
    let job = JobId::mint();
    let deployment = DeploymentId::mint();
    let operation = serde_json::to_string(&JobOperation::Reconcile {}).unwrap();
    let digest = "a".repeat(64);
    assert_eq!(
        runtime
            .execute(
                "INSERT INTO workflow_manager.queue_scopes(id) VALUES($1)",
                &[&app.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(runtime.execute(
        "INSERT INTO workflow_manager.jobs(id,app_id,deployment_id,operation,spec_digest,available_at,state,created_at) VALUES($1,$2,$3,$4,$5,0,'ready',0)",
        &[&job.as_str(), &app.as_str(), &deployment.as_str(), &operation, &digest],
    ).await.unwrap(), 1);
    let stored = runtime
        .query_one(
            "SELECT operation FROM workflow_manager.jobs WHERE app_id=$1 AND id=$2",
            &[&app.as_str(), &job.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(stored.get::<_, String>(0), operation);
    assert_eq!(
        runtime
            .execute(
                "UPDATE workflow_manager.jobs SET state='leased' WHERE app_id=$1 AND id=$2",
                &[&app.as_str(), &job.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        runtime
            .execute(
                "UPDATE workflow_manager.queue_scopes SET lock_version=lock_version+1 WHERE id=$1",
                &[&app.as_str()],
            )
            .await
            .unwrap(),
        1
    );

    for sql in [
        "SELECT * FROM customer.__zeroship_workflow_runs",
        "CREATE TABLE workflow_manager.extra(id text PRIMARY KEY)",
        "ALTER TABLE workflow_manager.jobs ADD COLUMN injected text",
        "DROP TABLE workflow_manager.jobs",
        "TRUNCATE workflow_manager.jobs",
    ] {
        let error = runtime.batch_execute(sql).await.unwrap_err();
        assert_eq!(
            error.as_db_error().unwrap().code().code(),
            "42501",
            "workflow runtime accepted {sql}: {error}"
        );
    }
    for privilege in [
        "TRUNCATE",
        "REFERENCES",
        "TRIGGER",
        "SELECT WITH GRANT OPTION",
        "INSERT WITH GRANT OPTION",
        "UPDATE WITH GRANT OPTION",
        "DELETE WITH GRANT OPTION",
    ] {
        for row in &tables {
            let table = format!("workflow_manager.{}", row.get::<_, String>(0));
            let granted = fixture
                .admin
                .query_one(
                    "SELECT has_table_privilege('zeroship_workflow', $1, $2)",
                    &[&table, &privilege],
                )
                .await
                .unwrap();
            assert!(
                !granted.get::<_, bool>(0),
                "runtime has {privilege} on {table}"
            );
        }
    }
    for role in [
        "zeroship_control",
        "zeroship_worker",
        "zeroship_gateway",
        "zeroship_app",
    ] {
        let schema = fixture
            .admin
            .query_one(
                "SELECT has_schema_privilege($1, 'workflow_manager', 'USAGE')",
                &[&role],
            )
            .await
            .unwrap();
        assert!(
            !schema.get::<_, bool>(0),
            "{role} can enter the queue namespace"
        );
        for row in &tables {
            let table = format!("workflow_manager.{}", row.get::<_, String>(0));
            for privilege in [
                "SELECT",
                "INSERT",
                "UPDATE",
                "DELETE",
                "TRUNCATE",
                "REFERENCES",
                "TRIGGER",
            ] {
                let granted = fixture
                    .admin
                    .query_one(
                        "SELECT has_table_privilege($1, $2, $3)",
                        &[&role, &table, &privilege],
                    )
                    .await
                    .unwrap();
                assert!(
                    !granted.get::<_, bool>(0),
                    "{role} has {privilege} on {table}"
                );
            }
        }
        fixture
            .admin
            .batch_execute(&format!("SET ROLE {role}"))
            .await
            .unwrap();
        let denied = fixture
            .admin
            .batch_execute("SELECT id FROM workflow_manager.jobs")
            .await;
        fixture.admin.batch_execute("RESET ROLE").await.unwrap();
        assert_eq!(
            denied.unwrap_err().as_db_error().unwrap().code().code(),
            "42501",
            "{role} can read the queue"
        );
    }
    assert_eq!(
        runtime
            .execute(
                "DELETE FROM workflow_manager.jobs WHERE app_id=$1 AND id=$2",
                &[&app.as_str(), &job.as_str()],
            )
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        runtime
            .execute(
                "DELETE FROM workflow_manager.queue_scopes WHERE id=$1",
                &[&app.as_str()],
            )
            .await
            .unwrap(),
        1
    );
}
