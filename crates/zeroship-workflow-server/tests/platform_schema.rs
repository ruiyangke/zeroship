//! Verify platform provisioning and workflow metadata database authority.
#[path = "support/holds.rs"]
mod holds;
#[path = "support/platform.rs"]
mod platform;

use std::rc::Rc;
use zeroship_workflow_server::coordinator::{connect_eligibility, Coordinator, Options};

/// The reserved prefix every generated journal object carries. It is what keeps
/// the journal and the manager's own coordination tables apart inside one
/// schema, and what a creator-declared collection is refused from.
const JOURNAL_PREFIX: &str = "__zeroship_workflow_";

/// The journal tables this build's generated descriptor declares, sorted.
///
/// Derived rather than listed: a hand-copied list here would stop describing
/// the artifact the moment the schema is regenerated, and would still pass.
fn journal_tables() -> Vec<String> {
    let descriptor: serde_json::Value =
        serde_json::from_str(zeroship_workflow_schema::RUNTIME_DESCRIPTOR_JSON)
            .expect("the generated workflow journal descriptor is JSON");
    let mut names = descriptor["collections"]
        .as_object()
        .expect("the journal descriptor declares collections")
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    assert!(
        !names.is_empty(),
        "the journal descriptor declares no tables"
    );
    for name in &names {
        assert!(
            name.starts_with(JOURNAL_PREFIX),
            "journal collection {name} is outside the reserved prefix"
        );
    }
    names.sort();
    names
}

/// The journal lives in the SERVICE's own schema, is stamped once for the whole
/// installation, and is read by nobody.
///
/// Installing it is the whole of this step. No role receives a privilege on it,
/// so an installation that also handed the runtime login - or any other - reach
/// over every app's workflow state fails here rather than passing as a step
/// that "works". The stamp is compared against the same constants the creator
/// bundle declares (`journal_bundle` in
/// `crates/zeroship-workflow-server/src/journal.rs`), so the two installation
/// sites cannot describe different journals.
async fn journal_is_installed_and_unread(fixture: &platform::Platform) {
    let stamp = fixture
        .admin
        .query(
            &format!(
                "SELECT id, version, fingerprint FROM workflow_manager.{}",
                zeroship_workflow_schema::STAMP_TABLE
            ),
            &[],
        )
        .await
        .unwrap();
    // ONE row per installation and not one per app: every app whose workflows
    // live in this schema is described by this single row, and the `app_id`
    // columns inside the journal are the tenant discriminator.
    assert_eq!(stamp.len(), 1, "the journal stamp is not a single row");
    assert_eq!(
        stamp[0].get::<_, String>("id"),
        zeroship_workflow_schema::STAMP_ROW_ID
    );
    assert_eq!(
        u32::try_from(stamp[0].get::<_, i64>("version")).unwrap(),
        zeroship_workflow_schema::VERSION
    );
    assert_eq!(
        stamp[0].get::<_, String>("fingerprint"),
        zeroship_workflow_schema::fingerprint(zeroship_workflow_schema::POSTGRES).unwrap()
    );

    let tables = journal_tables();
    for table in &tables {
        let qualified = format!("workflow_manager.{table}");
        for role in [
            "zeroship_workflow",
            "zeroship_control",
            "zeroship_worker",
            "zeroship_gateway",
            "zeroship_app",
        ] {
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
                        &[&role, &qualified, &privilege],
                    )
                    .await
                    .unwrap();
                assert!(
                    !granted.get::<_, bool>(0),
                    "{role} has {privilege} on {qualified}"
                );
            }
        }
    }

    // The journal the worker reads today lives in a CREATOR schema, installed
    // by the migration service's bundle path. This installation adds a second
    // site; it must not have moved the first, and it must not have scattered
    // journal tables through the platform's other schemas.
    let elsewhere = fixture
        .admin
        .query(
            "SELECT n.nspname, c.relname FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r' AND starts_with(c.relname::text, $1) \
             AND n.nspname IN ('zeroship', 'public', 'service_authn', 'zeroship_migrations')",
            &[&JOURNAL_PREFIX],
        )
        .await
        .unwrap();
    assert!(
        elsewhere.is_empty(),
        "journal tables were installed outside the workflow service's schema: {:?}",
        elsewhere
            .iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect::<Vec<_>>()
    );
    let creator = fixture
        .admin
        .query_one(
            "SELECT count(*) FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind = 'r' AND n.nspname = 'customer' AND c.relname = $1",
            &[&format!("{JOURNAL_PREFIX}runs")],
        )
        .await
        .unwrap();
    assert_eq!(
        creator.get::<_, i64>(0),
        1,
        "the creator-schema journal table this test seeded is gone"
    );
}

/// Which columns of the CORPUS-installed `workflow_manager` carry creator
/// payload, as a closed set.
///
/// `metadata_schema_has_no_customer_authority_and_ids_are_bytewise`
/// (`crates/zeroship-workflow-server/tests/coordinator.rs`) states this property
/// over a `workflow_manager` its own fixture builds from
/// `zeroship_workflow_server::coordinator::SCHEMA_SQL` and
/// `zeroship_workflow_manager::deployments::POSTGRES_SCHEMA`. The journal is not
/// in either, so that assertion reads a schema the journal never reached. This
/// runs the same predicate against the schema the migration corpus installs,
/// which is where `db/migrations-ts/20260919000000_workflow_journal.ts` puts it.
///
/// The set is closed, so a new json, jsonb or bytea column anywhere in
/// `workflow_manager`, or a new column named for a payload, fails here. The two
/// it names are the journal's inline generation payload;
/// `docs/proposals/2026-09-19-workflow-journal-relocation.md` settles that they
/// leave, and emptying this list is what that change looks like from here.
///
/// WHAT THIS DOES NOT SEE. The predicate matches a column's TYPE or its NAME, so
/// creator payload carried in a text column under another name passes it. The
/// `record` column of `__zeroship_workflow_steps` is a serialized
/// `StoredCheckpoint` wrapping the `StepCheckpoint` declared in
/// `crates/zeroship-workflow/src/engine.rs`, whose `output`, `error` and
/// `child_input` are creator values; `finish_run`
/// (`crates/zeroship-workflow/src/service/frontier.rs`) writes a creator error
/// into `__zeroship_workflow_generations.error` beside the two columns named
/// below; the `payload` columns of `__zeroship_workflow_signals` and
/// `__zeroship_workflow_broadcasts` take `options.payload` from the caller; and
/// `__zeroship_workflow_deploys.manifest` carries every `ScheduleRegistration`
/// input the deployment declared. An empty list here is not the same claim as a
/// payload-free journal.
async fn journal_payload_columns_are_a_closed_set(fixture: &platform::Platform) {
    let columns = fixture
        .admin
        .query(
            "SELECT table_name,column_name FROM information_schema.columns \
             WHERE table_schema='workflow_manager' AND (data_type IN ('json','jsonb','bytea') \
             OR column_name IN ('input','output','history','payload_url','database_url','task_token')) \
             ORDER BY table_name,column_name",
            &[],
        )
        .await
        .unwrap();
    assert_eq!(
        columns
            .iter()
            .map(|row| (row.get::<_, String>(0), row.get::<_, String>(1)))
            .collect::<Vec<_>>(),
        vec![
            (format!("{JOURNAL_PREFIX}generations"), "input".to_owned()),
            (format!("{JOURNAL_PREFIX}generations"), "output".to_owned()),
        ],
    );
}

#[ntex::test]
async fn platform_role_can_coordinate_without_customer_or_journal_privileges() {
    let fixture = platform::Platform::new().await;
    let eligibility = Rc::new(
        connect_eligibility(&fixture.runtime_url, Options::default())
            .await
            .unwrap(),
    );
    let service = Coordinator::connect(
        &fixture.runtime_url,
        Options::default(),
        holds::client(),
        eligibility,
    )
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
        "SELECT id,deploy_hash FROM zeroship.apps",
        "SELECT id,app_id,deploy_hash,retention_state FROM zeroship.app_deploys",
        "SELECT id,status,public_key FROM zeroship.worker_instances",
        "SELECT id,execution_zone_id,deleted_at FROM zeroship.apps",
        "SELECT id,execution_zone_id FROM zeroship.worker_instances",
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
        "SELECT public_key FROM zeroship.worker_join_signers",
        "SELECT id FROM zeroship.execution_zones",
        "UPDATE zeroship.worker_instances SET execution_zone_id='ezn_default000000000000000000'",
        "UPDATE zeroship.apps SET execution_zone_id='ezn_default000000000000000000'",
        "UPDATE zeroship.apps SET deploy_hash=NULL",
        "UPDATE zeroship.app_deploys SET retention_state='available'",
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
    journal_is_installed_and_unread(&fixture).await;
    journal_payload_columns_are_a_closed_set(&fixture).await;
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
    for table in [
        "management_scopes",
        "schedule_deployments",
        "schedule_activations",
        "schedule_disables",
        "schedule_scopes",
        "schedules",
        "schedule_occurrences",
        "recovery_scopes",
        "recovery_duties",
    ] {
        fixture
            .admin
            .batch_execute(&format!(
                "REVOKE UPDATE ON workflow_manager.{table} FROM zeroship_workflow"
            ))
            .await
            .unwrap();
        assert!(
            service.verify().await.is_err(),
            "missing driver privilege on {table}"
        );
        fixture
            .admin
            .batch_execute(&format!(
                "GRANT UPDATE ON workflow_manager.{table} TO zeroship_workflow"
            ))
            .await
            .unwrap();
        service.verify().await.unwrap();
    }
    for (table, columns) in [
        ("apps", "deploy_hash"),
        ("app_deploys", "id,app_id,deploy_hash,retention_state"),
    ] {
        fixture
            .admin
            .batch_execute(&format!(
                "REVOKE SELECT({columns}) ON zeroship.{table} FROM zeroship_workflow"
            ))
            .await
            .unwrap();
        assert!(
            service.verify().await.is_err(),
            "missing latest-deployment source privilege on {table}"
        );
        fixture
            .admin
            .batch_execute(&format!(
                "GRANT SELECT({columns}) ON zeroship.{table} TO zeroship_workflow"
            ))
            .await
            .unwrap();
        service.verify().await.unwrap();
    }
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
        recovery::{DutyKind, Options as RecoveryOptions, Recovery},
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
    for (kind, operation) in [
        (DutyKind::Reconcile, JobOperation::Reconcile {}),
        (DutyKind::Collect, JobOperation::Collect {}),
    ] {
        assert_eq!(
            recovery.due(kind, None).await.unwrap(),
            std::slice::from_ref(&app)
        );
        let job = recovery.dispatch(&app, kind).await.unwrap().unwrap();
        assert_eq!(job.app_id, app);
        assert_eq!(job.deployment_id(), None);
        assert_eq!(job.operation, operation);
        assert_eq!(recovery.dispatch(&app, kind).await.unwrap(), Some(job));
    }
    recovery_duty_constraints(fixture, &recovery, &app).await;
    let retained = fixture
        .admin
        .query_one(
            "SELECT count(*) FROM workflow_manager.deployment_holds WHERE app_id=$1",
            &[&app.as_str()],
        )
        .await
        .unwrap()
        .get::<_, i64>(0);
    assert_eq!(retained, 0);
}

#[expect(
    clippy::future_not_send,
    reason = "native duties and their constraint oracles share the compio database runtime"
)]
async fn recovery_duty_constraints(
    fixture: &platform::Platform,
    recovery: &zeroship_workflow_manager::recovery::Recovery,
    app: &zeroship_core::app_id::AppId,
) {
    use zeroship_core::{app_id::AppId, typed_id, workflow_jobs::DeploymentId};
    use zeroship_workflow_manager::recovery::DutyKind;

    let runtime = platform::connect(&fixture.runtime_url).await;
    let duties = runtime
        .query(
            "SELECT id,kind,pending_job_id FROM workflow_manager.recovery_duties \
             WHERE app_id=$1 ORDER BY kind",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    assert_eq!(duties.len(), 2);
    assert_eq!(duties[0].get::<_, &str>("kind"), "collect");
    assert_eq!(duties[1].get::<_, &str>("kind"), "reconcile");
    for duty in &duties {
        typed_id::parse_with_prefix(duty.get::<_, &str>("id"), "wrd").unwrap();
        assert!(duty.get::<_, Option<&str>>("pending_job_id").is_some());
    }
    let id = typed_id::generate("wrd");
    let duplicate = runtime
        .execute(
            "INSERT INTO workflow_manager.recovery_duties(id,app_id,kind,next_due_at) \
             VALUES($1,$2,'collect',0)",
            &[&id, &app.as_str()],
        )
        .await
        .unwrap_err();
    assert_eq!(duplicate.as_db_error().unwrap().code().code(), "23505");
    let missing_scope = runtime
        .execute(
            "INSERT INTO workflow_manager.recovery_duties(id,app_id,kind,next_due_at) \
             VALUES($1,$2,'collect',0)",
            &[&id, &AppId::mint().as_str()],
        )
        .await
        .unwrap_err();
    assert_eq!(missing_scope.as_db_error().unwrap().code().code(), "23503");

    let foreign = AppId::mint();
    recovery
        .ensure(&foreign, &DeploymentId::mint(), 1.try_into().unwrap())
        .await
        .unwrap();
    let foreign_job = recovery
        .dispatch(&foreign, DutyKind::Collect)
        .await
        .unwrap()
        .unwrap();
    let crossed = runtime
        .execute(
            "UPDATE workflow_manager.recovery_duties SET pending_job_id=$2 \
             WHERE app_id=$1 AND kind='collect'",
            &[&app.as_str(), &foreign_job.id.as_str()],
        )
        .await
        .unwrap_err();
    assert_eq!(crossed.as_db_error().unwrap().code().code(), "23503");
    let retained = recovery
        .dispatch(app, DutyKind::Collect)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        retained.id.as_str(),
        duties[0].get::<_, &str>("pending_job_id")
    );
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
    assert_eq!(page.jobs[0].deployment_id(), Some(&deployment));
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
    // The schema holds two platform artifacts with different authors: the
    // manager's own coordination tables, declared in the op DSL, and the
    // workflow journal, which is generated SQL behind the reserved
    // `__zeroship_` prefix. Partition rather than merge, so neither list can
    // absorb a stray table belonging to the other.
    let (mut journal, mut manager): (Vec<String>, Vec<String>) = tables
        .iter()
        .map(|row| row.get::<_, String>(0))
        .partition(|name| name.starts_with(JOURNAL_PREFIX));
    journal.sort();
    manager.sort();
    assert_eq!(journal, journal_tables());
    assert_eq!(
        manager,
        [
            "assignments",
            "capacity_demands",
            "capacity_targets",
            "deployment_holds",
            "jobs",
            "management",
            "management_scopes",
            "placement_receipts",
            "queue_scopes",
            "recovery_duties",
            "recovery_scopes",
            "schedule_activations",
            "schedule_deployments",
            "schedule_disables",
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
    let operation = serde_json::to_string(&JobOperation::Activate {
        deployment_id: deployment.clone(),
        revision: 1.try_into().unwrap(),
    })
    .unwrap();
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
        "INSERT INTO workflow_manager.jobs(id,app_id,deployment_id,operation,operation_kind,run_id,spec_digest,available_at,dispatch_order,state,created_at) VALUES($1,$2,$3,$4,'activate',NULL,$5,0,1,'ready',0)",
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
